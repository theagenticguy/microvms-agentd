// SPDX-License-Identifier: Apache-2.0
//! A local TCP listener that forwards HTTP and WebSocket traffic to one guest port.
//!
//! The client half of `microvm port-forward`. A caller binds a local address, and every
//! connection accepted there is replayed against the VM's endpoint with the two proxy
//! headers [`ProxyAuth`] mints for the guest port. What arrives at the guest is an ordinary
//! request from an ordinary client: the platform strips both headers before forwarding
//! (measured 2026-08-15, `docs/PLATFORM.md`), so a server inside the VM needs no MicroVM
//! awareness.
//!
//! # Why this is in core rather than in the CLI
//!
//! `microvms-cli/tests/thinness.rs` forbids `hyper`, `http`, and `reqwest` in the CLI's
//! dependency set, because each of them is a second path to AWS that does not go through
//! this crate. A forwarder is an HTTP server, so it cannot live there. Putting it here also
//! puts it behind the same [`ProxyAuth`] the session's own requests use, which is the
//! property TRAP-9 rests on: there is one mint schedule, and a long-lived tunnel opened at
//! minute fifty gets a token minted at minute fifty.
//!
//! # Every hop re-reads the header, and that is the refresh
//!
//! [`ProxyAuth::headers_for_port`] is called per proxied request rather than once at bind
//! time. The token's sixty-minute ceiling is therefore crossed by consulting the cache that
//! already knows about it — no timer here, no expiry arithmetic, and
//! [`ProxyAuth::mint_count`] is the observable that says a refresh happened. A forwarder
//! that captured the headers at startup would work for thirty minutes and then return 401s
//! that read as a wrong agent token.
//!
//! # An upgrade is forwarded as bytes, not as a protocol this module understands
//!
//! A WebSocket handshake is an ordinary GET whose response is 101. After that, neither end
//! speaks HTTP, so this module stops parsing and copies bytes in both directions until one
//! side closes. This code frames nothing after the handshake, so it cannot frame anything
//! wrongly, and an application subprotocol passes through untouched.
//!
//! # The upgrade path does not reach a guest server through the endpoint proxy yet
//!
//! Measured 2026-08-29, us-east-1, against a guest RFC 6455 echo server on port 8090. An
//! upgrade re-issued here as an ordinary HTTPS `GET` — `Upgrade: websocket` and
//! `Connection: Upgrade` forwarded as request headers — is answered by the **proxy** with
//! `400` (an `x-amzn-requestid` on the response, and the guest logged no handshake at all,
//! while the same handshake sent from inside the guest to `127.0.0.1:8090` answers 101).
//!
//! The reason is in `docs/PLATFORM.md`: on the endpoint the WebSocket credential travels as
//! **`Sec-WebSocket-Protocol` values**, not as the two proxy headers, because the browser
//! `WebSocket` constructor cannot set a header. So a working tunnel has to open a real
//! `wss://` handshake offering [`crate::session::Session::connect_subprotocols`]'s three
//! values, rather than replay the client's `GET` over the HTTPS path. That is a genuine
//! WebSocket client in this module, which the HTTP relay below is not.
//!
//! What *is* measured as working on that path, from the same run: a `wss://` handshake
//! offering the three minted subprotocols reaches the guest, and **binary** frames survive
//! byte-exact in both directions on a **port-scoped** token — including `0x00`/`0xFF`
//! payloads that a silent utf-8 round trip would corrupt, and a 300-byte frame that
//! exercises the extended-length header. The guest observed opcode `2` and no
//! `sec-websocket-protocol` header, so the proxy consumed all three values as documented.
//! That is the transport premise the raw-TCP tunnel (issue #70 layer 2) rests on, and it is
//! also what this module needs in order to carry an upgrade.
//!
//! Until that client exists, [`is_upgrade`] and [`splice`] are exercised by the tests below
//! against a local upstream, where the byte relay is correct — the untested edge is the
//! handshake negotiation with the platform, not the splice.
//!
//! # What a failure says
//!
//! The proxy answers **403** for a token whose scope does not cover the port and **502**
//! when the scope is right and nothing is listening in the guest. That pair is the only
//! diagnostic separating a scope mistake from a dead server, and on the WebSocket path both
//! collapse to close code 1006 with no reason. So this module surfaces the distinction from
//! the HTTP status while it still has one, in [`ForwardEvent::Refused`], rather than letting
//! a caller rediscover it from a closed socket.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::error::{Error, ErrorKind};
use crate::session::proxy::{PROXY_AUTH_HEADER, PROXY_PORT_HEADER, ProxyAuth};

/// How long one proxied exchange may take before it is abandoned.
///
/// Generous rather than tight: the thing on the other end is a developer's dev server, and a
/// long-polling request or a slow first compile is normal traffic rather than a fault. The
/// upgrade path is exempt — a WebSocket is idle by design, so once a connection is upgraded
/// this timeout no longer applies to it.
pub const DEFAULT_EXCHANGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// What the forwarder did, for a caller that wants to report progress.
///
/// An enum handed to a callback rather than a log line, because this crate does no logging
/// and the CLI owns every byte the user sees. The variants are the three things worth
/// telling somebody watching a tunnel: it is up, a request went through, or the proxy
/// refused one for a reason worth spelling out.
#[derive(Clone, Debug)]
pub enum ForwardEvent {
    /// The listener is bound and accepting. Carries the address actually bound, which
    /// differs from the requested one when the caller asked for port 0.
    Listening { local: SocketAddr, guest_port: u16 },
    /// One exchange completed with this status.
    Forwarded { status: u16, upgraded: bool },
    /// The proxy refused the exchange, with the distinction the status carries.
    ///
    /// `403` means the minted token's scope does not cover `guest_port`; `502` means the
    /// scope is right and nothing is listening there. Anything else is passed through
    /// unclassified rather than guessed at.
    Refused { status: u16, explanation: String },
    /// A connection ended in a way worth mentioning but not worth failing the tunnel for.
    ConnectionError { detail: String },
}

/// The 403-vs-502 sentence, or `None` for a status that needs no explaining.
///
/// Free function so the wording is tested without standing up a listener, and so the CLI
/// cannot drift from it by writing its own.
pub fn refusal_explanation(status: u16, guest_port: u16) -> Option<String> {
    match status {
        403 => Some(format!(
            "the endpoint proxy refused the request for port {guest_port} (403 Access to port \
             denied): the minted token's scope does not cover it. Nothing was wrong with the \
             guest — the credential never authorized this port."
        )),
        502 => Some(format!(
            "the endpoint proxy reached the VM and found nothing listening on port \
             {guest_port} (502). The credential is correct and the scope covers the port, so \
             this is a dead server in the guest rather than an auth problem."
        )),
        401 => Some(
            "the endpoint proxy rejected the credential (401). On a long-lived tunnel this \
             usually means a minted token expired without being refreshed; the mint count is \
             the observable that says whether a refresh happened."
                .to_string(),
        ),
        _ => None,
    }
}

/// Copies bytes both ways until one side closes, then returns.
///
/// Used for the post-101 tail of an upgraded connection. `copy_bidirectional` rather than
/// two hand-rolled loops: it already handles the half-close case where one direction ends
/// while the other still has data, which is the case a naive `select!` loop truncates.
pub async fn splice<A, B>(a: &mut A, b: &mut B) -> Result<(u64, u64), Error>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    tokio::io::copy_bidirectional(a, b).await.map_err(|err| {
        Error::new(
            ErrorKind::Unexpected,
            format!("the upgraded connection ended abnormally: {err}"),
        )
    })
}

/// Where the forwarder listens and what it forwards to.
///
/// A struct rather than four arguments for [`crate::seam`]'s reason: `local_port` and
/// `guest_port` are both `u16` and both plausible in either position, so a positional call
/// is how a tunnel ends up pointing at itself.
#[derive(Clone, Debug)]
pub struct ForwardSpec {
    /// The local address to bind. Port 0 asks the OS for a free one, which the
    /// [`ForwardEvent::Listening`] event then reports.
    pub bind: SocketAddr,
    /// The port inside the guest to forward to.
    pub guest_port: u16,
    /// The VM's endpoint host, as `run` reported it.
    pub endpoint: String,
    /// How long one non-upgraded exchange may take.
    pub exchange_timeout: std::time::Duration,
}

impl ForwardSpec {
    /// A spec with the default exchange timeout.
    pub fn new(bind: SocketAddr, guest_port: u16, endpoint: impl Into<String>) -> Self {
        Self {
            bind,
            guest_port,
            endpoint: endpoint.into(),
            exchange_timeout: DEFAULT_EXCHANGE_TIMEOUT,
        }
    }
}

/// The absolute URL a proxied request targets, given the endpoint and the request target.
///
/// A bare host is read as `https`, for the reason [`crate::session::http::ReqwestBackend`]
/// gives: the platform hands back a hostname, and defaulting to plain HTTP on the strength
/// of a missing prefix would put a bearer token on the wire in clear text.
pub fn upstream_url(endpoint: &str, target: &str) -> String {
    let base = endpoint.trim_end_matches('/');
    let base = if base.starts_with("https://") || base.starts_with("http://") {
        base.to_string()
    } else {
        format!("https://{base}")
    };
    if target.starts_with('/') {
        format!("{base}{target}")
    } else {
        format!("{base}/{target}")
    }
}

/// Header names this forwarder must never copy from the caller's request.
///
/// The two proxy headers are minted per hop, so a client that sent its own would either be
/// overridden silently or — worse — have its value forwarded and the mint skipped. Refusing
/// to copy them means the minted pair is the only pair, which is what makes the mint
/// schedule authoritative. `host` is dropped because the upstream host is the endpoint's,
/// not the local listener's.
pub const STRIPPED_REQUEST_HEADERS: [&str; 3] = [PROXY_AUTH_HEADER, PROXY_PORT_HEADER, "host"];

/// Whether a header from the local client is forwarded upstream.
pub fn forwards_request_header(name: &str) -> bool {
    !STRIPPED_REQUEST_HEADERS
        .iter()
        .any(|stripped| stripped.eq_ignore_ascii_case(name))
}

/// The HTTP client a forwarder's hops go through.
///
/// A newtype rather than a re-exported `reqwest::Client`, because the CLI is forbidden from
/// naming `reqwest` at all (`microvms-cli/tests/thinness.rs`) — so a caller there cannot build
/// one, and handing back the foreign type would only move the compile error. Pooled for the
/// whole tunnel: a page-load of thirty assets should not pay thirty TLS handshakes.
pub struct ForwardClient {
    inner: reqwest::Client,
}

impl ForwardClient {
    /// A pooled client for one tunnel.
    pub fn new() -> Result<Self, Error> {
        reqwest::Client::builder()
            .build()
            .map(|inner| Self { inner })
            .map_err(|err| {
                Error::new(
                    ErrorKind::Unexpected,
                    format!("could not build the forwarder's HTTP client: {err}"),
                )
            })
    }
}

impl std::fmt::Debug for ForwardClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForwardClient").finish_non_exhaustive()
    }
}

/// Binds the listener and reports the address, without serving anything yet.
///
/// Split from serving so a caller can print "listening on 8080" before the first
/// connection, and so a bind failure — the common one, an address already in use — is a
/// distinct error from a forwarding failure.
pub async fn bind(spec: &ForwardSpec) -> Result<TcpListener, Error> {
    TcpListener::bind(spec.bind).await.map_err(|err| {
        Error::new(
            ErrorKind::InvalidArg,
            format!(
                "could not bind {}: {err}. Another process is probably already listening \
                 there; pass a different local port.",
                spec.bind
            ),
        )
    })
}

/// The minted proxy headers for this spec's guest port, plus the `host` the endpoint wants.
///
/// One place, called per exchange. See the module docs on why this is not hoisted out of the
/// request path.
pub async fn hop_headers(
    auth: &Arc<ProxyAuth>,
    guest_port: u16,
) -> Result<Vec<(String, String)>, Error> {
    auth.headers_for_port(guest_port).await
}

/// Accepts one connection, for a caller driving the accept loop itself.
///
/// Exposed so the CLI owns the loop — and therefore owns Ctrl-C, the progress output, and
/// the decision to keep serving after one connection fails. A forwarder that owned its own
/// loop would have to grow a callback for each of those.
pub async fn accept(listener: &TcpListener) -> Result<(TcpStream, SocketAddr), Error> {
    listener.accept().await.map_err(|err| {
        Error::new(
            ErrorKind::Unexpected,
            format!("the local listener stopped accepting: {err}"),
        )
    })
}

/// Whether this request/response pair is a protocol upgrade the forwarder must splice.
///
/// Reads the **response** status rather than the request's `Upgrade` header, deliberately: a
/// client may offer an upgrade the guest declines, and in that case the exchange is ordinary
/// HTTP with an ordinary body. 101 is the only status that means "neither end speaks HTTP
/// from here", which is the only condition under which splicing bytes is correct.
pub fn is_upgrade(status: u16) -> bool {
    status == 101
}

/// One end of the local connection, as a duplex stream.
///
/// A trait alias in all but name, so the serving functions below take either a real
/// [`TcpStream`] or the in-memory duplex a test uses. Without it every test of the serving
/// path would need a real socket, and the failure modes worth testing — a refusal, an
/// upgrade, a mint inside the request path — have nothing to do with sockets.
pub trait LocalStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> LocalStream for T {}

/// Serves one local connection: reads its request, forwards it, and either relays the
/// response or splices an upgraded stream.
///
/// # The request is parsed, not tunnelled
///
/// A local client speaks cleartext HTTP to this listener while the upstream hop is HTTPS to
/// the endpoint, so the two cannot be a byte pipe: the request has to be understood well
/// enough to be re-issued with the minted headers, and the response well enough to know
/// whether 101 arrived. Once it has, parsing stops — see the module docs.
///
/// # A refusal is answered, not dropped
///
/// A 403 or 502 from the proxy becomes a plain-text HTTP response carrying
/// [`refusal_explanation`]'s sentence, so a browser renders the reason. Dropping the socket
/// would render as "the site can't be reached", which names the wrong component.
pub async fn serve_connection<S, F>(
    mut local: S,
    spec: &ForwardSpec,
    auth: &Arc<ProxyAuth>,
    client: &ForwardClient,
    mut on_event: F,
) -> Result<(), Error>
where
    S: LocalStream,
    F: FnMut(ForwardEvent),
{
    let head = match read_request_head(&mut local).await? {
        Some(head) => head,
        // A connection that closed before sending a request line. Browsers open and abandon
        // speculative connections constantly, so this is normal traffic and not an error.
        None => return Ok(()),
    };

    // Minted per exchange rather than once at bind time. This call is the token refresh —
    // see the module docs.
    let minted = hop_headers(auth, spec.guest_port).await?;

    let url = upstream_url(&spec.endpoint, &head.target);
    let method = reqwest::Method::from_bytes(head.method.as_bytes()).map_err(|err| {
        Error::new(
            ErrorKind::InvalidArg,
            format!(
                "the local client sent an unusable method {:?}: {err}",
                head.method
            ),
        )
    })?;

    let mut request = client
        .inner
        .request(method, &url)
        .timeout(spec.exchange_timeout);
    for (name, value) in head
        .headers
        .iter()
        .filter(|(name, _)| forwards_request_header(name))
    {
        request = request.header(name, value);
    }
    for (name, value) in &minted {
        request = request.header(name, value);
    }
    if !head.body.is_empty() {
        request = request.body(head.body.clone());
    }

    let response = request.send().await.map_err(|err| {
        Error::new(
            ErrorKind::Unexpected,
            format!(
                "forwarding {} {} to the endpoint failed: {err}",
                head.method, head.target
            ),
        )
    })?;
    let status = response.status().as_u16();

    if let Some(explanation) = refusal_explanation(status, spec.guest_port) {
        on_event(ForwardEvent::Refused {
            status,
            explanation: explanation.clone(),
        });
        write_local_error(&mut local, status, &explanation).await?;
        return Ok(());
    }

    if is_upgrade(status) {
        // Relay the 101 verbatim first: the client is waiting for the handshake response,
        // including whatever subprotocol the guest negotiated, before it will speak the
        // upgraded protocol.
        write_response_head(&mut local, status, response.headers()).await?;
        let mut upstream = response.upgrade().await.map_err(|err| {
            Error::new(
                ErrorKind::Unexpected,
                format!("the endpoint answered 101 but the connection did not upgrade: {err}"),
            )
        })?;
        on_event(ForwardEvent::Forwarded {
            status,
            upgraded: true,
        });
        splice(&mut local, &mut upstream).await?;
        return Ok(());
    }

    let headers = response.headers().clone();
    let body = response.bytes().await.map_err(|err| {
        Error::new(
            ErrorKind::Unexpected,
            format!("reading the guest's response body failed: {err}"),
        )
    })?;
    write_response(&mut local, status, &headers, &body).await?;
    on_event(ForwardEvent::Forwarded {
        status,
        upgraded: false,
    });
    Ok(())
}

/// A request head read off a local connection.
///
/// The body is collected here rather than streamed because a `content-length` body is what a
/// local client sends on this path — a `POST` to a dev server — and streaming it would mean
/// holding a half-read request across the mint. Chunked request bodies are not supported; a
/// caller sending one gets a named refusal rather than a truncated forward.
#[derive(Clone, Debug)]
struct RequestHead {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Reads one HTTP request head (and any `content-length` body) from a local stream.
///
/// `Ok(None)` for a connection that closed before a request line arrived, which browsers do
/// constantly with speculative connections.
async fn read_request_head<S>(local: &mut S) -> Result<Option<RequestHead>, Error>
where
    S: AsyncRead + Unpin + ?Sized,
{
    use tokio::io::AsyncReadExt as _;

    let mut buffer = Vec::new();
    let mut byte = [0_u8; 1];
    // Byte at a time until the header terminator. Slow in principle and irrelevant in
    // practice — a request head is a few hundred bytes — and it means never over-reading
    // into a body whose length the headers have not been parsed to discover yet.
    loop {
        match local.read(&mut byte).await {
            Ok(0) if buffer.is_empty() => return Ok(None),
            Ok(0) => {
                return Err(Error::new(
                    ErrorKind::Protocol,
                    "the local client closed the connection mid-request".to_string(),
                ));
            }
            Ok(_) => {
                buffer.push(byte[0]);
                if buffer.len() > MAX_REQUEST_HEAD_BYTES {
                    return Err(Error::new(
                        ErrorKind::InvalidArg,
                        format!(
                            "the local client's request head exceeded {MAX_REQUEST_HEAD_BYTES} \
                             bytes without terminating"
                        ),
                    ));
                }
                if buffer.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(err) => {
                return Err(Error::new(
                    ErrorKind::Unexpected,
                    format!("reading the local client's request failed: {err}"),
                ));
            }
        }
    }

    let text = String::from_utf8(buffer).map_err(|err| {
        Error::new(
            ErrorKind::Protocol,
            format!("the local client's request head is not utf-8: {err}"),
        )
    })?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    if method.is_empty() || target.is_empty() {
        return Err(Error::new(
            ErrorKind::Protocol,
            format!("the local client sent an unparseable request line {request_line:?}"),
        ));
    }

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(Error::new(
                ErrorKind::Protocol,
                format!("the local client sent an unparseable header line {line:?}"),
            ));
        };
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }

    if headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding") && value.contains("chunked")
    }) {
        return Err(Error::new(
            ErrorKind::InvalidArg,
            "the local client sent a chunked request body, which this forwarder does not \
             re-frame. Send a content-length body, or report the client that needs chunked \
             uploads so the forwarder can grow a streaming path."
                .to_string(),
        ));
    }

    let length: usize = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);

    let mut body = vec![0_u8; length];
    if length > 0 {
        local.read_exact(&mut body).await.map_err(|err| {
            Error::new(
                ErrorKind::Protocol,
                format!("the local client announced {length} body bytes and sent fewer: {err}"),
            )
        })?;
    }

    Ok(Some(RequestHead {
        method,
        target,
        headers,
        body,
    }))
}

/// The cap on a request head, so a client that never sends the terminator cannot grow the
/// buffer without bound. Generous: cookies and long URLs are ordinary.
const MAX_REQUEST_HEAD_BYTES: usize = 64 * 1024;

/// Headers a response must not carry back to the local client verbatim.
///
/// `content-length` and `transfer-encoding` are re-derived from the body actually written, so
/// copying the upstream values would risk announcing a length that disagrees with the bytes
/// — the failure that makes a browser hang waiting for a body that already ended.
const RECOMPUTED_RESPONSE_HEADERS: [&str; 2] = ["content-length", "transfer-encoding"];

/// Writes a status line and headers, without a body. Used for the 101 relay.
async fn write_response_head<W>(
    local: &mut W,
    status: u16,
    headers: &reqwest::header::HeaderMap,
) -> Result<(), Error>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason_phrase(status));
    for (name, value) in headers {
        if let Ok(value) = value.to_str() {
            head.push_str(&format!("{}: {}\r\n", name.as_str(), value));
        }
    }
    head.push_str("\r\n");
    local.write_all(head.as_bytes()).await.map_err(|err| {
        Error::new(
            ErrorKind::Unexpected,
            format!("could not write the response head to the local client: {err}"),
        )
    })?;
    local.flush().await.map_err(|err| {
        Error::new(
            ErrorKind::Unexpected,
            format!("could not flush the response head: {err}"),
        )
    })
}

/// Writes a full response, re-deriving the framing headers from `body`.
async fn write_response<W>(
    local: &mut W,
    status: u16,
    headers: &reqwest::header::HeaderMap,
    body: &[u8],
) -> Result<(), Error>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason_phrase(status));
    for (name, value) in headers {
        if RECOMPUTED_RESPONSE_HEADERS
            .iter()
            .any(|skip| skip.eq_ignore_ascii_case(name.as_str()))
        {
            continue;
        }
        if let Ok(value) = value.to_str() {
            head.push_str(&format!("{}: {}\r\n", name.as_str(), value));
        }
    }
    head.push_str(&format!("content-length: {}\r\n\r\n", body.len()));

    local.write_all(head.as_bytes()).await.map_err(|err| {
        Error::new(
            ErrorKind::Unexpected,
            format!("could not write the response to the local client: {err}"),
        )
    })?;
    local.write_all(body).await.map_err(|err| {
        Error::new(
            ErrorKind::Unexpected,
            format!("could not write the response body to the local client: {err}"),
        )
    })?;
    local.flush().await.map_err(|err| {
        Error::new(
            ErrorKind::Unexpected,
            format!("could not flush the response: {err}"),
        )
    })
}

/// A reason phrase for the statuses this forwarder writes itself.
///
/// Only the ones it originates need to be right; everything else is relayed from upstream
/// where the phrase is cosmetic (HTTP/1.1 clients read the code, not the phrase).
fn reason_phrase(status: u16) -> &'static str {
    match status {
        101 => "Switching Protocols",
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

/// Writes an HTTP error response to a local client whose exchange could not be proxied.
///
/// Plain text and a real status rather than dropping the socket: a browser pointed at a
/// tunnel whose token lost its scope should render the reason, and a dropped connection
/// renders as "the site can't be reached", which names the wrong component.
pub async fn write_local_error<W>(stream: &mut W, status: u16, body: &str) -> Result<(), Error>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let reason = match status {
        403 => "Forbidden",
        502 => "Bad Gateway",
        504 => "Gateway Timeout",
        _ => "Bad Gateway",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/plain; charset=utf-8\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.map_err(|err| {
        Error::new(
            ErrorKind::Unexpected,
            format!("could not answer the local client: {err}"),
        )
    })?;
    stream.flush().await.map_err(|err| {
        Error::new(
            ErrorKind::Unexpected,
            format!("could not flush the local client's response: {err}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::proxy::DEFAULT_AGENT_PORT;
    use crate::session::proxy::testing::{CountingMinter, ManualClock};

    /// A [`ProxyAuth`] over the shared counting minter, so a test can read the mint count and
    /// the scopes that were asked for.
    fn auth_pair() -> (Arc<ProxyAuth>, Arc<CountingMinter>) {
        let minter = Arc::new(CountingMinter::default());
        let auth = ProxyAuth::with_refresh_after(
            minter.clone(),
            DEFAULT_AGENT_PORT,
            std::time::Duration::from_secs(30 * 60),
            Arc::new(ManualClock::default()),
        )
        .expect("the default refresh interval is below the ceiling");
        (Arc::new(auth), minter)
    }

    /// A one-shot upstream HTTP/1.1 server on loopback.
    ///
    /// Real sockets rather than a mocked reqwest, because what these tests check is that the
    /// bytes reqwest puts on the wire carry the minted headers — and a fake client would be
    /// asserting against the fake. Returns the bound address and a handle yielding the request
    /// text the server saw.
    async fn upstream(response: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
        let addr = listener.local_addr().expect("bound");
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("one connection");
            let head = read_request_head(&mut socket)
                .await
                .expect("a readable request")
                .expect("a request arrived");
            socket
                .write_all(response.as_bytes())
                .await
                .expect("the canned response is written");
            socket.flush().await.expect("flushed");
            let headers = head
                .headers
                .iter()
                .map(|(name, value)| format!("{}: {value}", name.to_ascii_lowercase()))
                .collect::<Vec<_>>()
                .join("\n");
            format!("{} {}\n{headers}", head.method, head.target)
        });
        (format!("http://{addr}"), handle)
    }

    /// Drives one exchange through [`serve_connection`] and returns what the local client saw
    /// plus every event the forwarder reported.
    async fn exchange(
        request: &str,
        endpoint: String,
        guest_port: u16,
        auth: &Arc<ProxyAuth>,
    ) -> (String, Vec<ForwardEvent>) {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let spec = ForwardSpec::new(
            "127.0.0.1:0".parse().expect("an addr"),
            guest_port,
            endpoint,
        );
        let http = ForwardClient::new().expect("a client");

        let auth = auth.clone();
        let served = tokio::spawn(async move {
            let mut events = Vec::new();
            let result =
                serve_connection(server, &spec, &auth, &http, |event| events.push(event)).await;
            (result, events)
        });

        client
            .write_all(request.as_bytes())
            .await
            .expect("the request is written");
        client.flush().await.expect("flushed");

        let (result, events) = served.await.expect("the task joins");
        result.expect("the exchange completes");

        let mut seen = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut client, &mut seen)
            .await
            .expect("the client reads the response");
        (String::from_utf8_lossy(&seen).to_string(), events)
    }

    #[tokio::test]
    async fn an_ordinary_get_reaches_the_guest_carrying_both_minted_headers() {
        // The whole point of layer 1: a local client's request arrives upstream with the
        // port-scoped credential attached, and the guest's response comes back intact.
        let (endpoint, upstream_saw) =
            upstream("HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello").await;
        let (auth, minter) = auth_pair();

        let (seen, events) = exchange(
            "GET /index.html HTTP/1.1\r\nhost: localhost:8080\r\nuser-agent: probe\r\n\r\n",
            endpoint,
            8080,
            &auth,
        )
        .await;

        let request = upstream_saw.await.expect("the upstream task joins");
        assert!(request.starts_with("GET /index.html"), "{request}");
        assert!(
            request.contains(&format!(
                "{}: jwe-0",
                PROXY_AUTH_HEADER.to_ascii_lowercase()
            )),
            "the minted auth header did not reach upstream: {request}"
        );
        assert!(
            request.contains(&format!("{}: 8080", PROXY_PORT_HEADER.to_ascii_lowercase())),
            "the port header did not name the guest port: {request}"
        );
        assert!(request.contains("user-agent: probe"), "{request}");

        assert!(seen.starts_with("HTTP/1.1 200 OK\r\n"), "{seen}");
        assert!(seen.ends_with("hello"), "{seen}");
        assert!(
            matches!(
                events.as_slice(),
                [ForwardEvent::Forwarded {
                    status: 200,
                    upgraded: false
                }]
            ),
            "{events:?}"
        );

        // The scope asked for is the guest port, not the agent port alone — the 2026-08-15
        // defect. `mint_for_ports` records what was requested.
        let requested = minter.requested();
        assert!(
            requested.iter().any(|ports| ports.contains(&8080)),
            "the mint never asked for port 8080: {requested:?}"
        );
    }

    #[tokio::test]
    async fn the_caller_cannot_smuggle_its_own_proxy_headers_upstream() {
        // Falsification: drop the `forwards_request_header` filter in `serve_connection` and
        // the forged value arrives beside the minted one, which is a request whose credential
        // the caller chose.
        let (endpoint, upstream_saw) =
            upstream("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok").await;
        let (auth, _minter) = auth_pair();

        let request = format!(
            "GET / HTTP/1.1\r\nhost: local\r\n{PROXY_AUTH_HEADER}: forged-jwe\r\n\
             {PROXY_PORT_HEADER}: 1\r\n\r\n"
        );
        let (_seen, _events) = exchange(&request, endpoint, 8080, &auth).await;

        let saw = upstream_saw.await.expect("the upstream task joins");
        assert!(
            !saw.contains("forged-jwe"),
            "a caller's forged credential was forwarded: {saw}"
        );
        assert!(
            saw.contains(&format!("{}: 8080", PROXY_PORT_HEADER.to_ascii_lowercase())),
            "the minted port header should be the only one: {saw}"
        );
        assert!(
            !saw.contains("host: local"),
            "the local host header must not override the endpoint's: {saw}"
        );
    }

    #[tokio::test]
    async fn a_scope_refusal_is_answered_with_the_sentence_that_names_it() {
        // 403 is the one status that means "the credential never authorized this port", and a
        // browser must render that rather than "the site can't be reached".
        let (endpoint, upstream_saw) =
            upstream("HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\n\r\n").await;
        let (auth, _minter) = auth_pair();

        let (seen, events) = exchange(
            "GET / HTTP/1.1\r\nhost: local\r\n\r\n",
            endpoint,
            8080,
            &auth,
        )
        .await;
        let _ = upstream_saw.await;

        assert!(seen.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{seen}");
        assert!(seen.contains("scope does not cover"), "{seen}");
        match events.as_slice() {
            [
                ForwardEvent::Refused {
                    status: 403,
                    explanation,
                },
            ] => {
                assert!(explanation.contains("8080"), "{explanation}");
            }
            other => panic!("expected one 403 refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_dead_guest_server_is_reported_as_502_rather_than_as_an_auth_problem() {
        let (endpoint, upstream_saw) =
            upstream("HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\n\r\n").await;
        let (auth, _minter) = auth_pair();

        let (seen, events) = exchange(
            "GET / HTTP/1.1\r\nhost: local\r\n\r\n",
            endpoint,
            5432,
            &auth,
        )
        .await;
        let _ = upstream_saw.await;

        assert!(seen.contains("nothing listening on port 5432"), "{seen}");
        assert!(
            matches!(
                events.as_slice(),
                [ForwardEvent::Refused { status: 502, .. }]
            ),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn the_guests_own_404_is_relayed_and_not_reinterpreted() {
        // A 404 from the dev server is the dev server's answer. A forwarder that explained it
        // would be inventing a diagnosis for someone else's status.
        let (endpoint, upstream_saw) =
            upstream("HTTP/1.1 404 Not Found\r\ncontent-length: 9\r\n\r\nno-route!").await;
        let (auth, _minter) = auth_pair();

        let (seen, events) = exchange(
            "GET /nope HTTP/1.1\r\nhost: local\r\n\r\n",
            endpoint,
            8080,
            &auth,
        )
        .await;
        let _ = upstream_saw.await;

        assert!(seen.starts_with("HTTP/1.1 404 Not Found\r\n"), "{seen}");
        assert!(seen.ends_with("no-route!"), "{seen}");
        assert!(
            matches!(
                events.as_slice(),
                [ForwardEvent::Forwarded { status: 404, .. }]
            ),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_post_body_is_forwarded_and_the_response_length_is_re_derived() {
        // The upstream announces a content-length; the forwarder must write one that matches
        // the bytes it actually relays, or a browser hangs waiting for a body that ended.
        let (endpoint, upstream_saw) = upstream(
            "HTTP/1.1 200 OK\r\ncontent-length: 3\r\ntransfer-encoding: identity\r\n\r\nack",
        )
        .await;
        let (auth, _minter) = auth_pair();

        let (seen, _events) = exchange(
            "POST /submit HTTP/1.1\r\nhost: local\r\ncontent-length: 7\r\n\r\npayload",
            endpoint,
            8080,
            &auth,
        )
        .await;
        let saw = upstream_saw.await.expect("the upstream task joins");

        assert!(saw.starts_with("POST /submit"), "{saw}");
        assert!(saw.contains("content-length: 7"), "{saw}");
        assert_eq!(
            seen.matches("content-length:").count(),
            1,
            "exactly one framing header should be written: {seen}"
        );
        assert!(seen.contains("content-length: 3\r\n"), "{seen}");
        assert!(
            !seen.to_ascii_lowercase().contains("transfer-encoding"),
            "the upstream framing header must not be copied: {seen}"
        );
        assert!(seen.ends_with("ack"), "{seen}");
    }

    #[tokio::test]
    async fn a_101_relays_the_handshake_and_then_stops_parsing() {
        // The WebSocket path. The 101 and its negotiated subprotocol reach the client
        // verbatim, and everything after it is bytes in both directions — this module frames
        // nothing, so it cannot frame anything wrongly.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
        let addr = listener.local_addr().expect("bound");
        let guest = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("one connection");
            let _ = read_request_head(&mut socket)
                .await
                .expect("a request")
                .expect("present");
            socket
                .write_all(
                    b"HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\n\
                      connection: upgrade\r\nsec-websocket-protocol: my-app-protocol\r\n\r\n",
                )
                .await
                .expect("the handshake is written");
            socket.flush().await.expect("flushed");
            // Now speak the upgraded protocol: read the client's frame, answer it.
            let mut frame = [0_u8; 5];
            tokio::io::AsyncReadExt::read_exact(&mut socket, &mut frame)
                .await
                .expect("the client's bytes arrive");
            socket
                .write_all(b"PONG")
                .await
                .expect("the answer is written");
            socket.flush().await.expect("flushed");
            frame.to_vec()
        });

        let (auth, _minter) = auth_pair();
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let spec = ForwardSpec::new(
            "127.0.0.1:0".parse().expect("an addr"),
            8080,
            format!("http://{addr}"),
        );
        let http = ForwardClient::new().expect("a client");
        let served = tokio::spawn(async move {
            let mut events = Vec::new();
            let result =
                serve_connection(server, &spec, &auth, &http, |event| events.push(event)).await;
            (result, events)
        });

        client
            .write_all(
                b"GET /socket HTTP/1.1\r\nhost: local\r\nupgrade: websocket\r\n\
                  connection: upgrade\r\nsec-websocket-protocol: my-app-protocol\r\n\r\n",
            )
            .await
            .expect("the handshake request is written");
        client.flush().await.expect("flushed");

        // The handshake response arrives before any upgraded byte, which is what lets a
        // client know the negotiation succeeded.
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            tokio::io::AsyncReadExt::read_exact(&mut client, &mut byte)
                .await
                .expect("the handshake response arrives");
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head).to_string();
        assert!(
            head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
            "{head}"
        );
        assert!(
            head.to_ascii_lowercase()
                .contains("sec-websocket-protocol: my-app-protocol"),
            "the guest's negotiated subprotocol must reach the client: {head}"
        );

        client
            .write_all(b"PING!")
            .await
            .expect("a frame is written");
        client.flush().await.expect("flushed");
        let mut answer = [0_u8; 4];
        tokio::io::AsyncReadExt::read_exact(&mut client, &mut answer)
            .await
            .expect("the guest's answer arrives");
        assert_eq!(&answer, b"PONG");

        drop(client);
        let guest_saw = guest.await.expect("the guest task joins");
        assert_eq!(
            &guest_saw, b"PING!",
            "the client's bytes reached the guest unframed"
        );

        let (result, events) = served.await.expect("the task joins");
        result.expect("the splice ends cleanly");
        assert!(
            events.iter().any(|event| matches!(
                event,
                ForwardEvent::Forwarded {
                    status: 101,
                    upgraded: true
                }
            )),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn every_exchange_mints_through_the_cache_rather_than_capturing_headers_once() {
        // TRAP-9's shape for a tunnel: the credential is re-read per hop, so crossing the
        // sixty-minute ceiling is the cache's job and not a timer here. Two exchanges over one
        // ProxyAuth must both go through it.
        let (auth, minter) = auth_pair();

        for _ in 0..2 {
            let (endpoint, upstream_saw) =
                upstream("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok").await;
            let (_seen, _events) = exchange(
                "GET / HTTP/1.1\r\nhost: local\r\n\r\n",
                endpoint,
                8080,
                &auth,
            )
            .await;
            let saw = upstream_saw.await.expect("the upstream task joins");
            assert!(
                saw.contains(&PROXY_AUTH_HEADER.to_ascii_lowercase()),
                "every hop carries the minted credential: {saw}"
            );
        }

        // One mint, two hops: the second read the cache. That is the correct behaviour — the
        // assertion that matters is that neither hop went out uncredentialed, and that the
        // count is the observable saying whether a refresh happened.
        assert_eq!(
            auth.mint_count(),
            1,
            "the second hop should reuse the cached token rather than re-mint"
        );
        assert_eq!(minter.requested().len(), 1);
    }

    #[tokio::test]
    async fn a_chunked_request_body_is_refused_by_name_rather_than_truncated() {
        // This forwarder does not re-frame chunked bodies. Silently forwarding the head and
        // dropping the body would corrupt an upload in a way the user cannot see.
        let (mut client, mut server) = tokio::io::duplex(4096);
        client
            .write_all(b"POST /up HTTP/1.1\r\nhost: local\r\ntransfer-encoding: chunked\r\n\r\n")
            .await
            .expect("written");
        client.flush().await.expect("flushed");

        let error = read_request_head(&mut server)
            .await
            .expect_err("chunked is refused");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        assert!(error.to_string().contains("chunked"), "{error}");
    }

    #[tokio::test]
    async fn a_connection_that_closes_before_sending_anything_is_not_an_error() {
        // Browsers open speculative connections and abandon them. Treating that as a failure
        // would print an error per tab.
        let (client, mut server) = tokio::io::duplex(4096);
        drop(client);
        let head = read_request_head(&mut server)
            .await
            .expect("an empty connection is fine");
        assert!(head.is_none());
    }

    #[tokio::test]
    async fn an_unterminated_request_head_is_capped_rather_than_buffered_without_bound() {
        let (mut client, mut server) = tokio::io::duplex(1024 * 1024);
        let writer = tokio::spawn(async move {
            // Never sends the terminator.
            let filler = vec![b'x'; MAX_REQUEST_HEAD_BYTES + 1024];
            let _ = client.write_all(b"GET / HTTP/1.1\r\nx: ").await;
            let _ = client.write_all(&filler).await;
            let _ = client.flush().await;
        });

        let error = read_request_head(&mut server)
            .await
            .expect_err("the cap fires");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        assert!(error.to_string().contains("without terminating"), "{error}");
        writer.abort();
    }

    #[test]
    fn only_a_101_counts_as_an_upgrade() {
        // A client may offer an upgrade the guest declines, and then the exchange is ordinary
        // HTTP with an ordinary body. Reading the request's Upgrade header instead would
        // splice a connection that still had a body to relay.
        assert!(is_upgrade(101));
        assert!(!is_upgrade(200));
        assert!(!is_upgrade(426));
    }

    #[test]
    fn a_bare_endpoint_host_is_read_as_https() {
        // The reason is in the module docs: defaulting to http on a missing prefix puts a
        // bearer token on the wire in clear text.
        assert_eq!(
            upstream_url("vm-abc.example.aws", "/index.html"),
            "https://vm-abc.example.aws/index.html"
        );
    }

    #[test]
    fn an_explicit_scheme_is_left_alone() {
        assert_eq!(
            upstream_url("https://vm-abc.example.aws/", "/a"),
            "https://vm-abc.example.aws/a"
        );
        assert_eq!(
            upstream_url("http://127.0.0.1:9000", "/a"),
            "http://127.0.0.1:9000/a"
        );
    }

    #[test]
    fn a_target_without_a_leading_slash_still_forms_one_path() {
        assert_eq!(
            upstream_url("vm.example", "index.html"),
            "https://vm.example/index.html"
        );
    }

    #[test]
    fn the_two_proxy_headers_are_never_copied_from_the_caller() {
        // A client that sent its own X-aws-proxy-auth must not have it forwarded: the
        // minted pair is the only pair, which is what keeps the mint schedule authoritative.
        assert!(!forwards_request_header("x-aws-proxy-auth"));
        assert!(!forwards_request_header("X-AWS-PROXY-AUTH"));
        assert!(!forwards_request_header("x-aws-proxy-port"));
        assert!(!forwards_request_header("Host"));
        assert!(forwards_request_header("user-agent"));
        assert!(forwards_request_header("sec-websocket-protocol"));
    }

    #[test]
    fn the_refusal_wording_separates_a_scope_mistake_from_a_dead_server() {
        // The one diagnostic pair PLATFORM.md names, and the reason this function exists
        // rather than the CLI writing its own sentence.
        let scope = refusal_explanation(403, 8080).expect("403 explains itself");
        assert!(scope.contains("scope does not cover"), "{scope}");
        assert!(scope.contains("8080"), "{scope}");

        let dead = refusal_explanation(502, 8080).expect("502 explains itself");
        assert!(dead.contains("nothing listening"), "{dead}");
        assert!(dead.contains("8080"), "{dead}");

        assert!(
            refusal_explanation(401, 8080).is_some(),
            "401 is worth naming"
        );
        assert!(
            refusal_explanation(200, 8080).is_none(),
            "a success explains nothing"
        );
        assert!(
            refusal_explanation(404, 8080).is_none(),
            "the guest's own 404 is not ours"
        );
    }

    #[tokio::test]
    async fn a_bind_collision_names_the_address_and_suggests_another_port() {
        let held = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
        let addr = held.local_addr().expect("a bound address");

        let spec = ForwardSpec::new(addr, 8080, "vm.example");
        let error = bind(&spec).await.expect_err("the address is taken");
        assert_eq!(error.kind(), ErrorKind::InvalidArg);
        assert!(error.to_string().contains(&addr.to_string()), "{error}");
        assert!(
            error.to_string().contains("different local port"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_local_error_response_is_a_readable_http_message() {
        // A browser pointed at a scope-refused tunnel should render the reason; a dropped
        // socket renders as "the site can't be reached", which names the wrong component.
        let (mut client, mut server) = tokio::io::duplex(4096);
        let body = refusal_explanation(403, 8080).expect("wording");
        write_local_error(&mut server, 403, &body)
            .await
            .expect("the response is written");
        drop(server);

        let mut got = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut client, &mut got)
            .await
            .expect("the client reads it");
        let text = String::from_utf8(got).expect("utf-8");
        assert!(text.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{text}");
        assert!(text.contains("content-length: "), "{text}");
        assert!(text.ends_with(&body), "{text}");
    }

    #[tokio::test]
    async fn splice_copies_both_directions_and_reports_the_byte_counts() {
        let (mut left_near, mut left_far) = tokio::io::duplex(4096);
        let (mut right_near, mut right_far) = tokio::io::duplex(4096);

        let pump = tokio::spawn(async move { splice(&mut left_far, &mut right_near).await });

        left_near.write_all(b"to-guest").await.expect("write");
        left_near.shutdown().await.expect("half close");
        right_far.write_all(b"from-guest!").await.expect("write");
        right_far.shutdown().await.expect("half close");

        let mut to_guest = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut right_far, &mut to_guest)
            .await
            .expect("read");
        let mut from_guest = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut left_near, &mut from_guest)
            .await
            .expect("read");

        let (a_to_b, b_to_a) = pump.await.expect("the task joins").expect("a clean splice");
        assert_eq!(to_guest, b"to-guest");
        assert_eq!(from_guest, b"from-guest!");
        assert_eq!(a_to_b, 8);
        assert_eq!(b_to_a, 11);
    }
}
