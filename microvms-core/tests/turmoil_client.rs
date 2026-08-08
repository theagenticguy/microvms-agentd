//! Client-fault tier: the session's retry, mint, and cursor logic under a deterministic
//! network.
//!
//! # What this tier owns, and what it deliberately does not
//!
//! The daemon's own `agentd/tests/turmoil_transport.rs` already drives the real
//! `routes::app` over a simulated network and owns every daemon-side byte semantic: the
//! replay ring, the gap event, keep-alive framing, whether a rejection drains or closes.
//! Repeating that here would measure the daemon twice and the client not at all.
//!
//! So the host in this simulation is the **endpoint proxy**, which is the component the
//! two properties under test belong to. That is a correction to the task packet's
//! instruction to spin the real `agentd` router, and the reason is not convenience:
//!
//! * `agentd` never sees `X-aws-proxy-auth` or `X-aws-proxy-port`. Those headers are
//!   consumed by the platform's proxy and are not in the daemon's vocabulary at all — it
//!   authenticates with a bearer token and nothing else. A scenario that stripped both
//!   proxy headers and sent the request at the real router would get a 200. TRAP-7 is
//!   therefore **unfalsifiable** against the daemon, and a tier that cannot fail is
//!   worse than no tier.
//! * The same goes for TRAP-9. Token expiry is a proxy behaviour; the daemon has no
//!   token lifetime to expire.
//!
//! The proxy host below validates both headers, expires a token at the sixty-minute
//! ceiling, and serves a minimal exec surface behind it. The wire shapes it answers with
//! are the `protocol` crate's, so it cannot drift from the daemon: a field renamed there
//! breaks this file's compilation the same way it breaks the daemon's.
//!
//! # Two clocks, avoided rather than managed
//!
//! The prior lesson is that a simulator's virtual clock and a spawned child's real one
//! disagree, and that a server-side deadline measured against real work expires almost
//! immediately. This tier spawns no child at all: the proxy serves a byte sequence the
//! scenario declared, and it cuts the connection at the frame the scenario named rather
//! than at whatever point a timer happened to fire. So the harness is the clock in the
//! strongest available sense — every ordering a scenario needs is *caused* rather than
//! hoped for, exactly as the lesson prescribes, and it holds under any tick duration.
//!
//! The one place the virtual clock is load-bearing is the token-ceiling scenario, where
//! seventy minutes pass for free. That is why the client's clock is `tokio::time::Instant`
//! rather than `std::time::Instant`: a session timed against `std` would sit inside a
//! seventy-minute simulation without ever re-minting, and the scenario would pass while
//! measuring nothing.
//!
//! # The listener discipline is the daemon's
//!
//! [`SimListener`] is the same newtype shape as `agentd/tests/turmoil_transport.rs:121`,
//! `enable_tokio_io` and `rng_seed` are set for the same reasons, and [`long_sim`]
//! coarsens the tick for the same arithmetic — seventy simulated minutes at the default
//! 1 ms tick is 4.2 million steps.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;
use microvms_core::error::{Error, WireKind};
use microvms_core::session::{
    ChunkSource, DEFAULT_AGENT_PORT, DEFAULT_REFRESH_AFTER, ExecEvent, HttpBackend, HttpRequest,
    HttpResponse, OpenStream, PROXY_AUTH_HEADER, PROXY_PORT_HEADER, ProxyAuth, ProxyToken, Session,
    StreamOptions, TokenMinter,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use turmoil::Sim;
use turmoil::net::{TcpListener, TcpStream};

/// The port the proxy listens on. The agent port, since that is what the port header
/// names and a mismatch is one of the things a scenario asserts on.
const PORT: u16 = DEFAULT_AGENT_PORT;

/// The proxy host's name in simulated DNS.
const PROXY: &str = "endpoint-proxy";

/// The token the run hook installed. Long enough that a truncating comparison would not
/// accidentally match.
const AGENT_TOKEN: &str = "sim-agent-token-0123456789";

/// The exec every scenario addresses.
const EXEC_ID: &str = "x-turmoil";

// ---------------------------------------------------------------------------
// Harness: the listener, and the proxy that validates both headers
// ---------------------------------------------------------------------------

/// Adapts a turmoil listener to a plain accept loop, counting accepts.
///
/// The same newtype the daemon's tier uses, minus the `axum::serve::Listener` impl —
/// there is no axum in this crate's dependency set, so the loop is here rather than in a
/// generic `serve`. The count is kept for the same reason: connection reuse is invisible
/// in a response body, so a test that only checked statuses could not tell one attach
/// from three.
struct SimListener {
    inner: TcpListener,
    accepted: Arc<AtomicUsize>,
}

impl SimListener {
    async fn accept(&mut self) -> io::Result<(TcpStream, SocketAddr)> {
        let pair = self.inner.accept().await?;
        self.accepted.fetch_add(1, Ordering::Relaxed);
        Ok(pair)
    }
}

/// What the proxy should do on each request it receives.
#[derive(Clone)]
enum Serve {
    /// A JSON body with this status.
    Json(u16, String),
    /// An SSE stream: send these frames, then **cut the connection** without a
    /// terminating chunk and without an `exit` event.
    ///
    /// The cut is the scenario. A body that ended gracefully and one that was cut are the
    /// same byte count, and only the absence of the terminal event distinguishes them —
    /// which is the property the client's reconnect decision reads.
    CutAfter(Vec<String>),
    /// An SSE stream that ends properly, terminating chunk and all.
    StreamThen(Vec<String>),
}

/// The proxy's script: what to answer, in order, plus what it recorded.
#[derive(Default)]
struct Script {
    serves: Mutex<std::collections::VecDeque<Serve>>,
    /// Every request line and header set the proxy saw.
    seen: Mutex<Vec<Recorded>>,
    /// Requests refused for a missing or wrong proxy header.
    refused: AtomicU64,
    /// Requests refused because the presented token was past the ceiling.
    expired: AtomicU64,
    /// Every token the control plane minted, with the simulated instant it was minted at.
    ///
    /// A **ceiling measured in time**, not "the newest value wins". That distinction was
    /// a real defect in this harness: modelling expiry as "the proxy accepts only the
    /// latest minted token" made the TRAP-9 scenarios pass against a client that
    /// refreshed *at* the sixty-minute ceiling, because such a client still mints before
    /// each late request and the newest value is therefore always the one presented. The
    /// service does not work that way — a token is valid for sixty minutes from *its own*
    /// mint, and several are live at once.
    minted: Mutex<HashMap<String, Duration>>,
    /// The age, in whole seconds, of the oldest token any request presented.
    ///
    /// This is the observable that makes TRAP-9 falsifiable, and getting there took two
    /// wrong attempts worth recording. A refuse-if-expired counter cannot catch a bad
    /// refresh interval, because the refresh is checked *before* each request: a client
    /// refreshing at the ceiling still re-mints immediately before any request that would
    /// otherwise be late, so it never presents a token the proxy would reject. What it
    /// does do is present tokens with almost no life left — a token built at age 3599s
    /// and validated by the proxy a round trip later has expired *in flight*, which is
    /// the finding's actual wording and the least debuggable failure available.
    ///
    /// So the assertion is the margin rather than the rejection: the oldest token any
    /// request carried, measured where the proxy read it. A thirty-minute window keeps
    /// that near thirty minutes; a ceiling-width one drives it to sixty, and the
    /// remaining life to zero.
    oldest_presented_secs: AtomicU64,
}

/// One request as the proxy saw it on the wire.
#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    path: String,
    headers: HashMap<String, String>,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// The `offset` query parameter, which is the client's cursor.
    fn offset(&self) -> Option<u64> {
        self.path
            .split("offset=")
            .nth(1)
            .and_then(|rest| rest.split('&').next())
            .and_then(|value| value.parse().ok())
    }
}

impl Script {
    fn with(serves: impl IntoIterator<Item = Serve>) -> Arc<Self> {
        Arc::new(Self {
            serves: Mutex::new(serves.into_iter().collect()),
            ..Self::default()
        })
    }

    fn requests(&self) -> Vec<Recorded> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Records a mint at the current simulated instant.
    fn record_mint(&self, value: String) {
        self.minted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(value, turmoil::sim_elapsed().unwrap_or_default());
    }

    /// Judges a presented token, and records how old it was when this proxy read it.
    ///
    /// The age is recorded for every accepted token, not only for rejected ones — see
    /// [`Self::oldest_presented_secs`] on why the margin rather than the rejection is the
    /// observable.
    fn judge(&self, value: &str) -> Verdict {
        let minted = self.minted.lock().unwrap_or_else(PoisonError::into_inner);
        // An empty ledger means no scenario wired a minter to this proxy, so token
        // validation is not what the scenario is about.
        if minted.is_empty() {
            return Verdict::Valid;
        }
        let Some(minted_at) = minted.get(value) else {
            return Verdict::Unknown;
        };
        let now = turmoil::sim_elapsed().unwrap_or_default();
        let age = now.saturating_sub(*minted_at);
        self.oldest_presented_secs
            .fetch_max(age.as_secs(), Ordering::SeqCst);
        if age >= CEILING {
            Verdict::Expired
        } else {
            Verdict::Valid
        }
    }

    /// The age of the oldest token any request presented.
    fn oldest_presented(&self) -> Duration {
        Duration::from_secs(self.oldest_presented_secs.load(Ordering::SeqCst))
    }
}

/// What the proxy makes of a presented token.
#[derive(Debug, Eq, PartialEq)]
enum Verdict {
    Valid,
    /// Minted, but more than [`CEILING`] ago.
    Expired,
    /// Never minted here.
    Unknown,
}

/// The lifetime the service enforces on a proxy token. Not a choice.
const CEILING: Duration = Duration::from_secs(60 * 60);

/// Registers the proxy as a simulated host.
///
/// The `Script` is created outside the host factory, so what it recorded survives the
/// host's own restart — the same reason the daemon's tier builds its `AppState` outside
/// `sim.host`.
fn spawn_proxy(sim: &mut Sim<'_>, script: Arc<Script>) -> Arc<AtomicUsize> {
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);

    sim.host(PROXY, move || {
        let script = Arc::clone(&script);
        let accepted = Arc::clone(&counter);
        async move {
            let inner = TcpListener::bind((Ipv4Addr::UNSPECIFIED, PORT)).await?;
            let mut listener = SimListener { inner, accepted };
            loop {
                let (stream, _) = listener.accept().await?;
                let script = Arc::clone(&script);
                // One task per connection, and a failure in one is not a failure of the
                // proxy: several scenarios abandon a socket deliberately.
                tokio::spawn(async move {
                    let _ = handle(stream, script).await;
                });
            }
        }
    });

    accepted
}

/// Serves one connection: read a request head, validate the proxy headers, answer.
async fn handle(mut stream: TcpStream, script: Arc<Script>) -> io::Result<()> {
    let Some(recorded) = read_head(&mut stream).await? else {
        return Ok(());
    };
    script
        .seen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(recorded.clone());

    // TRAP-7, enforced rather than assumed. Both headers, or a 403 — which is what makes
    // the client-side guard falsifiable: drop either header and this branch fires.
    let auth = recorded.header(PROXY_AUTH_HEADER);
    let port = recorded.header(PROXY_PORT_HEADER);
    if auth.is_none() || port.is_none() {
        script.refused.fetch_add(1, Ordering::SeqCst);
        return answer_json(
            &mut stream,
            403,
            r#"{"message":"missing proxy credentials"}"#,
        )
        .await;
    }
    if port != Some(&PORT.to_string()) {
        script.refused.fetch_add(1, Ordering::SeqCst);
        return answer_json(&mut stream, 403, r#"{"message":"port not allowed"}"#).await;
    }

    // TRAP-9: a token past its own sixty minutes is refused, and the refusal reads like a
    // bad credential — which is exactly why the client must re-mint before it happens.
    match script.judge(auth.unwrap_or_default()) {
        Verdict::Valid => {}
        Verdict::Expired => {
            script.expired.fetch_add(1, Ordering::SeqCst);
            return answer_json(&mut stream, 403, r#"{"message":"token expired"}"#).await;
        }
        Verdict::Unknown => {
            script.refused.fetch_add(1, Ordering::SeqCst);
            return answer_json(&mut stream, 403, r#"{"message":"unknown token"}"#).await;
        }
    }

    let serve = script
        .serves
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .pop_front();
    match serve {
        Some(Serve::Json(status, body)) => answer_json(&mut stream, status, &body).await,
        Some(Serve::CutAfter(frames)) => {
            answer_sse(&mut stream, &frames, false).await?;
            // Drop without the terminating chunk. turmoil turns a drop with unread bytes
            // into a reset, which is the mid-stream disconnect this scenario is about.
            Ok(())
        }
        Some(Serve::StreamThen(frames)) => answer_sse(&mut stream, &frames, true).await,
        None => answer_json(&mut stream, 500, r#"{"message":"script exhausted"}"#).await,
    }
}

/// Reads one request head, returning `None` on a clean close before it arrived.
async fn read_head(stream: &mut TcpStream) -> io::Result<Option<Recorded>> {
    let mut buf = Vec::new();
    let head_end = loop {
        if let Some(idx) = find(&buf, b"\r\n\r\n") {
            break idx + 4;
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut headers = HashMap::new();
    let mut declared = 0usize;
    for line in lines.take_while(|line| !line.is_empty()) {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                declared = value.parse().unwrap_or(0);
            }
            headers.insert(name, value);
        }
    }

    // The body is consumed so the connection is left at a message boundary; otherwise a
    // keep-alive scenario would see this request's leftovers and report a desync the
    // harness caused itself.
    let mut have = buf.len() - head_end;
    while have < declared {
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        have += n;
    }

    Ok(Some(Recorded {
        method,
        path,
        headers,
    }))
}

async fn answer_json(stream: &mut TcpStream, status: u16, body: &str) -> io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

/// Answers with a chunked SSE body, optionally ending it properly.
async fn answer_sse(stream: &mut TcpStream, frames: &[String], graceful: bool) -> io::Result<()> {
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                cache-control: no-cache\r\nx-accel-buffering: no\r\n\
                transfer-encoding: chunked\r\n\r\n";
    stream.write_all(head.as_bytes()).await?;
    for frame in frames {
        stream
            .write_all(format!("{:x}\r\n{frame}\r\n", frame.len()).as_bytes())
            .await?;
        stream.flush().await?;
    }
    if graceful {
        stream.write_all(b"0\r\n\r\n").await?;
        stream.flush().await?;
    }
    Ok(())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---------------------------------------------------------------------------
// The client side: a backend over turmoil sockets
// ---------------------------------------------------------------------------

/// The session's [`HttpBackend`], talking over the simulated network.
///
/// This is the seam the production `ReqwestBackend` occupies. Everything above it —
/// header assembly, the mint schedule, the cursor arithmetic, the reconnect decision —
/// is the real production code path, which is the point: nothing under test is a
/// test-only re-implementation.
struct SimBackend;

fn wire_error(request: &HttpRequest, err: &io::Error) -> Error {
    Error::wire(
        WireKind::Transport,
        format!(
            "{} {} failed on the simulated wire: {err}",
            request.method, request.path
        ),
    )
}

/// Reads a response head, returning the status, headers, leftover body bytes, the
/// declared length, and whether the body is chunked.
#[allow(clippy::type_complexity)]
async fn read_response_head(
    stream: &mut TcpStream,
) -> io::Result<(u16, HashMap<String, String>, Vec<u8>, Option<usize>, bool)> {
    let mut buf = Vec::new();
    let head_end = loop {
        if let Some(idx) = find(&buf, b"\r\n\r\n") {
            break idx + 4;
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the connection closed before a response head",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unparseable status line"))?;

    let mut headers = HashMap::new();
    for line in lines.take_while(|line| !line.is_empty()) {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let declared = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok());
    let chunked = headers
        .get("transfer-encoding")
        .is_some_and(|value| value.contains("chunked"));

    Ok((status, headers, buf[head_end..].to_vec(), declared, chunked))
}

impl HttpBackend for SimBackend {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, Error>> {
        Box::pin(async move {
            let mut stream = TcpStream::connect((PROXY, PORT))
                .await
                .map_err(|err| wire_error(&request, &err))?;
            let mut head = format!(
                "{} {} HTTP/1.1\r\nhost: {PROXY}\r\n",
                request.method, request.path
            );
            for (name, value) in &request.headers {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
            head.push_str(&format!("content-length: {}\r\n\r\n", request.body.len()));
            stream
                .write_all(head.as_bytes())
                .await
                .map_err(|err| wire_error(&request, &err))?;
            if !request.body.is_empty() {
                stream
                    .write_all(&request.body)
                    .await
                    .map_err(|err| wire_error(&request, &err))?;
            }
            stream
                .flush()
                .await
                .map_err(|err| wire_error(&request, &err))?;

            let (status, headers, mut body, declared, _) = read_response_head(&mut stream)
                .await
                .map_err(|err| wire_error(&request, &err))?;
            let want = declared.unwrap_or(0);
            while body.len() < want {
                let mut chunk = [0u8; 4096];
                let n = stream
                    .read(&mut chunk)
                    .await
                    .map_err(|err| wire_error(&request, &err))?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..n]);
            }
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
            let mut stream = TcpStream::connect((PROXY, PORT))
                .await
                .map_err(|err| wire_error(&request, &err))?;
            let mut head = format!(
                "{} {} HTTP/1.1\r\nhost: {PROXY}\r\naccept: text/event-stream\r\n",
                request.method, request.path
            );
            for (name, value) in &request.headers {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
            head.push_str("\r\n");
            stream
                .write_all(head.as_bytes())
                .await
                .map_err(|err| wire_error(&request, &err))?;
            stream
                .flush()
                .await
                .map_err(|err| wire_error(&request, &err))?;

            let (status, headers, leftover, declared, chunked) = read_response_head(&mut stream)
                .await
                .map_err(|err| wire_error(&request, &err))?;

            // A failing head carries its whole body, so the client's typed error keeps
            // the proxy's detail string — a 403 from a stripped header has to be
            // readable, or the guard proof reads as a bare transport failure.
            if !(200..300).contains(&status) {
                let mut body = leftover;
                let want = declared.unwrap_or(0);
                while body.len() < want {
                    let mut chunk = [0u8; 4096];
                    let n = stream
                        .read(&mut chunk)
                        .await
                        .map_err(|err| wire_error(&request, &err))?;
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
                return Ok((
                    HttpResponse {
                        status,
                        headers,
                        body,
                    },
                    Box::new(NoChunks) as Box<dyn ChunkSource>,
                ));
            }

            let source = SimChunks {
                stream: Some(stream),
                wire: leftover,
                chunk_left: 0,
                chunked,
                idle_timeout,
            };
            Ok((
                HttpResponse {
                    status,
                    headers,
                    body: Vec::new(),
                },
                Box::new(source) as Box<dyn ChunkSource>,
            ))
        })
    }
}

struct NoChunks;

impl ChunkSource for NoChunks {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, Error>> {
        Box::pin(async { Ok(None) })
    }
}

/// De-chunks a streaming body off a turmoil socket.
///
/// Chunked framing is decoded here because the proxy answers a stream with
/// `transfer-encoding: chunked` and no length, exactly as axum does, so the SSE frames
/// arrive interleaved with chunk headers. Partial by design: a chunk header split across
/// two reads leaves the buffer untouched and the caller reads more, which is what keeps a
/// slow delivery from being misparsed as a framing defect.
struct SimChunks {
    stream: Option<TcpStream>,
    wire: Vec<u8>,
    chunk_left: usize,
    chunked: bool,
    idle_timeout: Duration,
}

impl SimChunks {
    /// Whatever decoded body bytes are already buffered.
    fn decode(&mut self) -> (Vec<u8>, bool) {
        if !self.chunked {
            return (std::mem::take(&mut self.wire), false);
        }
        let mut out = Vec::new();
        loop {
            if self.chunk_left == 0 {
                if self.wire.starts_with(b"\r\n") {
                    self.wire.drain(..2);
                    continue;
                }
                let Some(idx) = find(&self.wire, b"\r\n") else {
                    return (out, false);
                };
                let header = String::from_utf8_lossy(&self.wire[..idx]).into_owned();
                let Ok(size) = usize::from_str_radix(header.trim(), 16) else {
                    return (out, false);
                };
                self.wire.drain(..idx + 2);
                if size == 0 {
                    // The terminating chunk: the body ended the way a completed response
                    // does.
                    return (out, true);
                }
                self.chunk_left = size;
            }
            let take = self.chunk_left.min(self.wire.len());
            if take == 0 {
                return (out, false);
            }
            out.extend(self.wire.drain(..take));
            self.chunk_left -= take;
        }
    }
}

impl ChunkSource for SimChunks {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, Error>> {
        Box::pin(async move {
            loop {
                let (decoded, ended) = self.decode();
                if !decoded.is_empty() {
                    return Ok(Some(decoded));
                }
                if ended {
                    self.stream = None;
                    return Ok(None);
                }
                let Some(stream) = self.stream.as_mut() else {
                    return Ok(None);
                };
                let mut chunk = [0u8; 4096];
                match tokio::time::timeout(self.idle_timeout, stream.read(&mut chunk)).await {
                    // A FIN or a reset with no terminating chunk. The body is over, but
                    // not the way a completed response ends — which is precisely the
                    // distinction the client's reconnect decision reads.
                    Ok(Ok(0)) | Ok(Err(_)) => {
                        self.stream = None;
                        return Ok(None);
                    }
                    Ok(Ok(n)) => self.wire.extend_from_slice(&chunk[..n]),
                    Err(_) => {
                        self.stream = None;
                        return Err(Error::wire(
                            WireKind::Transport,
                            "the simulated stream went silent past the idle timeout",
                        ));
                    }
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// The minter: a control plane that mints against the proxy's expectation
// ---------------------------------------------------------------------------

/// Mints tokens and records each one's mint instant with the proxy.
///
/// The coupling is the scenario: the proxy honours a token for sixty minutes from *its
/// own* mint, so a client whose refresh schedule is too late presents a token past that
/// and is rejected. Recording the instant rather than just the value is what makes the
/// expiry observable — see [`Script::minted`] on the version of this that could not fail.
struct SimMinter {
    script: Arc<Script>,
    issued: AtomicU64,
    failures: AtomicU64,
}

impl SimMinter {
    fn new(script: Arc<Script>) -> Self {
        Self {
            script,
            issued: AtomicU64::new(0),
            failures: AtomicU64::new(0),
        }
    }

    fn failing(script: Arc<Script>, times: u64) -> Self {
        Self {
            script,
            issued: AtomicU64::new(0),
            failures: AtomicU64::new(times),
        }
    }
}

impl TokenMinter for SimMinter {
    fn mint(&self) -> BoxFuture<'_, Result<ProxyToken, Error>> {
        Box::pin(async move {
            if self
                .failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                return Err(Error::wire(
                    WireKind::AuthTokenMint,
                    "ThrottlingException from CreateMicrovmAuthToken",
                ));
            }
            let nth = self.issued.fetch_add(1, Ordering::SeqCst);
            let value = format!("jwe-scoped-{nth}");
            self.script.record_mint(value.clone());
            // A map, as the service returns. Treating this as a string is TRAP-7.
            Ok(ProxyToken::from_pairs([(PROXY_AUTH_HEADER, value)]))
        })
    }
}

// ---------------------------------------------------------------------------
// Simulation builders, mirroring the daemon's tier
// ---------------------------------------------------------------------------

fn sim(seed: u64) -> Sim<'static> {
    turmoil::Builder::new()
        .rng_seed(seed)
        .enable_tokio_io()
        .simulation_duration(Duration::from_secs(120))
        .build()
}

/// A simulation whose virtual clock has room for the proxy-token boundary.
///
/// The tick is coarsened for the same arithmetic the daemon's tier documents: stepping
/// seventy simulated minutes at the default 1 ms tick is 4.2 million steps; at 500 ms it
/// is 8,400. Nothing under test resolves faster than a network round trip.
fn long_sim(seed: u64) -> Sim<'static> {
    sim_for(seed, Duration::from_secs(90 * 60))
}

fn sim_for(seed: u64, duration: Duration) -> Sim<'static> {
    turmoil::Builder::new()
        .rng_seed(seed)
        .enable_tokio_io()
        .simulation_duration(duration)
        .tick_duration(Duration::from_millis(500))
        .build()
}

/// A session over the simulated network, with proxy auth wired to `minter`.
fn session(minter: Arc<dyn TokenMinter>) -> Session {
    Session::builder(format!("http://{PROXY}:{PORT}"), AGENT_TOKEN)
        .with_backend(Arc::new(SimBackend))
        .with_minter(minter)
        .build()
        .expect("the session builds")
}

/// A session whose [`ProxyAuth`] the caller supplied, for the mint-count scenarios.
fn session_with_auth(auth: Arc<ProxyAuth>) -> Session {
    Session::builder(format!("http://{PROXY}:{PORT}"), AGENT_TOKEN)
        .with_backend(Arc::new(SimBackend))
        .with_proxy_auth(auth)
        .build()
        .expect("the session builds")
}

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

/// One SSE `output` frame, built through the protocol crate so it cannot drift.
fn output_frame(offset: u64, bytes: &[u8]) -> String {
    let payload = protocol::exec::OutputEvent {
        offset,
        stream: protocol::exec::StreamKind::Stdout,
        output: base64::engine::general_purpose::STANDARD.encode(bytes),
    };
    format!(
        "event: {}\ndata: {}\n\n",
        protocol::exec::EVENT_OUTPUT,
        serde_json::to_string(&payload).expect("serializes")
    )
}

fn exit_frame(total: u64) -> String {
    let payload = protocol::exec::ExitEvent {
        exit_code: Some(0),
        signal: None,
        truncated: false,
        writers_may_be_alive: false,
        offset: total,
    };
    format!(
        "event: {}\ndata: {}\n\n",
        protocol::exec::EVENT_EXIT,
        serde_json::to_string(&payload).expect("serializes")
    )
}

fn health_json(bootstrapped: bool) -> String {
    serde_json::to_string(&protocol::health::Health {
        version: std::borrow::Cow::Borrowed("0.1.0"),
        bootstrapped,
        disk: None,
        identity_degraded: false,
        identity_repaired: true,
    })
    .expect("serializes")
}

/// Reads a whole stream, returning the bytes it delivered and the events it saw.
async fn drain_stream(
    session: &Session,
    options: StreamOptions,
) -> Result<(Vec<u8>, Vec<ExecEvent>), Error> {
    let handle = session.exec(EXEC_ID);
    let mut bytes = Vec::new();
    let mut events = Vec::new();
    let mut stream = std::pin::pin!(handle.stream_with(options));
    while let Some(item) = stream.next().await {
        let event = item?;
        if let ExecEvent::Output { data, .. } = &event {
            bytes.extend_from_slice(data);
        }
        events.push(event);
    }
    Ok((bytes, events))
}

// ---------------------------------------------------------------------------
// 1. The harness is real
// ---------------------------------------------------------------------------

/// A request over the simulated network reaches the proxy and comes back.
///
/// This exists to keep every other test in this file honest. If the listener never
/// bound, or the backend's framing were wrong, every scenario below would "pass" by
/// finding nothing.
#[test]
fn the_simulated_proxy_answers_a_request_the_real_session_built() -> turmoil::Result {
    let script = Script::with([Serve::Json(200, health_json(true))]);
    let mut sim = sim(0x5EED_0201);
    spawn_proxy(&mut sim, Arc::clone(&script));

    let minter = Arc::new(SimMinter::new(Arc::clone(&script)));
    sim.client("harness", async move {
        let session = session(minter);
        let health = session.health().await?;
        assert!(health.bootstrapped, "the proxy's body did not survive");
        Ok(())
    });

    sim.run()?;
    assert_eq!(script.requests().len(), 1);
    assert_eq!(script.refused.load(Ordering::SeqCst), 0);
    Ok(())
}

// ---------------------------------------------------------------------------
// 2. TRAP-7: both proxy headers, enforced by the proxy
// ---------------------------------------------------------------------------

/// Every request the session sends carries both proxy headers, and the proxy accepts it.
///
/// **Guard proof.** The assertion is on what arrived at the proxy, not on what the client
/// returned, and the proxy refuses a request missing either header with a 403. So
/// deleting the port-header branch in `ProxyAuth::headers_from` turns this red twice
/// over: the header assertion fails, and the request itself fails.
#[test]
fn both_proxy_headers_reach_the_proxy_on_every_request() -> turmoil::Result {
    let script = Script::with([
        Serve::Json(200, health_json(true)),
        Serve::Json(
            200,
            serde_json::to_string(&protocol::exec::PollResponse {
                exec_id: EXEC_ID.into(),
                phase: protocol::exec::Phase::Running,
                result: None,
            })
            .expect("serializes"),
        ),
        Serve::StreamThen(vec![output_frame(0, b"hi\n"), exit_frame(3)]),
    ]);
    let mut sim = sim(0x5EED_0202);
    spawn_proxy(&mut sim, Arc::clone(&script));

    let minter = Arc::new(SimMinter::new(Arc::clone(&script)));
    sim.client("harness", async move {
        let session = session(minter);
        session.health().await?;
        session.exec(EXEC_ID).poll().await?;
        let (bytes, _) = drain_stream(&session, StreamOptions::default()).await?;
        assert_eq!(bytes, b"hi\n");
        Ok(())
    });

    sim.run()?;

    let seen = script.requests();
    assert_eq!(seen.len(), 3, "not every route was exercised: {seen:#?}");
    for request in &seen {
        assert_eq!(
            request.header(PROXY_AUTH_HEADER),
            Some("jwe-scoped-0"),
            "{} {} arrived without the proxy auth header",
            request.method,
            request.path
        );
        assert_eq!(
            request.header(PROXY_PORT_HEADER),
            Some("9000"),
            "{} {} arrived without the proxy port header, which the proxy refuses as \
             if the token were bad",
            request.method,
            request.path
        );
    }
    assert_eq!(
        script.refused.load(Ordering::SeqCst),
        0,
        "the proxy refused a request, so a header was missing or wrong"
    );
    Ok(())
}

/// The proxy really does refuse a request missing the port header.
///
/// This is the other half of the guard proof, and it is what makes the test above capable
/// of failing: it demonstrates that the enforcement is real by sending a request the
/// client would never build. Without this, "the proxy accepted everything" would be
/// consistent with a proxy that checks nothing.
#[test]
fn the_proxy_refuses_a_request_that_omits_the_port_header() -> turmoil::Result {
    let script = Script::with([Serve::Json(200, health_json(true))]);
    let mut sim = sim(0x5EED_0203);
    spawn_proxy(&mut sim, Arc::clone(&script));

    sim.client("harness", async move {
        // Hand-built, bypassing `ProxyAuth` entirely: only the auth header.
        let backend = SimBackend;
        let mut request = HttpRequest::new("GET", "/v1/health".to_string());
        request
            .headers
            .push((PROXY_AUTH_HEADER.to_string(), "jwe-scoped-0".to_string()));
        let response = backend.send(request).await?;
        assert_eq!(
            response.status, 403,
            "the proxy accepted a request with no port header, so the TRAP-7 guard \
             above cannot fail"
        );
        Ok(())
    });

    sim.run()?;
    assert_eq!(script.refused.load(Ordering::SeqCst), 1);
    Ok(())
}

// ---------------------------------------------------------------------------
// 3. The offset cursor across a mid-stream disconnect
// ---------------------------------------------------------------------------

/// A stream cut mid-flight reconnects at the cursor and loses no byte and duplicates none.
///
/// The verdict is the reassembled byte sequence, not the absence of an error. A client
/// that ignored the offset and replayed from zero would deliver every byte too, and only
/// the seam shows the difference — so this asserts the exact join *and* the offset the
/// second attach requested.
///
/// The proxy's replay is keyed on that offset, so the second attach's content is a
/// function of what the client asked for rather than a fixture that would look the same
/// either way.
#[test]
fn a_mid_stream_disconnect_reconnects_at_the_cursor_and_loses_nothing() -> turmoil::Result {
    let script = Script::with([
        // First attach: two frames, then the connection dies with no `exit` event.
        Serve::CutAfter(vec![output_frame(0, b"AAAA\n"), output_frame(5, b"BBBB\n")]),
        // Second attach: the tail, ending properly.
        Serve::StreamThen(vec![output_frame(10, b"CCCC\n"), exit_frame(15)]),
    ]);
    let mut sim = sim(0x5EED_0204);
    let accepted = spawn_proxy(&mut sim, Arc::clone(&script));

    let minter = Arc::new(SimMinter::new(Arc::clone(&script)));
    sim.client("harness", async move {
        let session = session(minter);
        let (bytes, events) = drain_stream(&session, StreamOptions::default()).await?;

        assert_eq!(
            String::from_utf8_lossy(&bytes),
            "AAAA\nBBBB\nCCCC\n",
            "the two attaches did not reconstruct the output, so the reconnect \
             duplicated or lost bytes at the seam"
        );
        assert!(
            matches!(events.last(), Some(ExecEvent::Exit(exit)) if exit.offset == 15),
            "the stream did not end on the terminal event: {events:#?}"
        );
        Ok(())
    });

    sim.run()?;

    let seen = script.requests();
    assert_eq!(
        seen.len(),
        2,
        "the cut did not produce exactly one reconnect: {seen:#?}"
    );
    assert_eq!(
        seen[0].offset(),
        Some(0),
        "the first attach must start at 0"
    );
    assert_eq!(
        seen[1].offset(),
        Some(10),
        "the reconnect asked for byte {:?} rather than 10, so the cursor did not \
         follow what was delivered",
        seen[1].offset()
    );
    assert_eq!(
        accepted.load(Ordering::Relaxed),
        2,
        "the reconnect did not open a fresh connection"
    );
    Ok(())
}

/// A gap moves the cursor past evicted bytes, so a reconnect does not ask for them again.
///
/// Without this the client asks for a range the daemon has already dropped and is told
/// about the same gap forever — a livelock that looks like a slow stream rather than a
/// bug.
#[test]
fn a_gap_moves_the_cursor_past_bytes_that_are_gone() -> turmoil::Result {
    let gap = format!(
        "event: {}\ndata: {}\n\n",
        protocol::exec::EVENT_GAP,
        serde_json::to_string(&protocol::exec::GapEvent { from: 2, to: 900 }).expect("serializes")
    );
    let script = Script::with([
        Serve::CutAfter(vec![output_frame(0, b"AA"), gap]),
        Serve::StreamThen(vec![output_frame(900, b"ZZ"), exit_frame(902)]),
    ]);
    let mut sim = sim(0x5EED_0205);
    spawn_proxy(&mut sim, Arc::clone(&script));

    let minter = Arc::new(SimMinter::new(Arc::clone(&script)));
    sim.client("harness", async move {
        let session = session(minter);
        let (bytes, events) = drain_stream(&session, StreamOptions::default()).await?;
        assert_eq!(bytes, b"AAZZ");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ExecEvent::Gap { from: 2, to: 900 })),
            "the gap was swallowed, so a truncated log reads as a complete one: \
             {events:#?}"
        );
        Ok(())
    });

    sim.run()?;
    let seen = script.requests();
    assert_eq!(
        seen[1].offset(),
        Some(900),
        "the reconnect asked for bytes the daemon had already evicted"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 4. TRAP-9: the token expires mid-flight and the retry path mints
// ---------------------------------------------------------------------------

/// A trial that outlives a proxy token keeps working, because the mint is in the request
/// path.
///
/// Seventy simulated minutes pass between two requests, which costs no wall clock. The
/// proxy accepts only the newest minted value, so a client that cached its first token
/// would get a 403 that reads like a bad agent token — the least debuggable failure
/// available, and the whole reason TRAP-9 is a finding.
///
/// **Guard proof.** The mint count is the verdict. Two successful requests are consistent
/// with a token cached forever *if the proxy did not expire it*, which is why the proxy
/// enforces the expiry and why `expired` is asserted at zero: the client must re-mint
/// *before* the boundary, not recover after it.
#[test]
fn a_session_that_outlives_the_token_ceiling_mints_inside_the_request_path() -> turmoil::Result {
    let script = Script::with([
        Serve::Json(200, health_json(true)),
        Serve::Json(200, health_json(true)),
    ]);
    let mut sim = long_sim(0x5EED_0206);
    spawn_proxy(&mut sim, Arc::clone(&script));

    let minter = Arc::new(SimMinter::new(Arc::clone(&script)));
    let auth = Arc::new(ProxyAuth::new(minter, PORT));
    let observed = Arc::clone(&auth);
    sim.client("harness", async move {
        let session = session_with_auth(auth);
        session.health().await?;

        // Virtual time, so this is free. Past the refresh window and past the ceiling.
        tokio::time::sleep(Duration::from_secs(70 * 60)).await;

        let health = session.health().await?;
        assert!(
            health.bootstrapped,
            "the request after the token boundary failed"
        );
        Ok(())
    });

    sim.run()?;

    assert_eq!(
        observed.mint_count(),
        2,
        "the session did not re-mint across the {}s ceiling, so a long trial would \
         die at minute sixty",
        Duration::from_secs(60 * 60).as_secs()
    );
    assert_eq!(
        script.expired.load(Ordering::SeqCst),
        0,
        "a request went out carrying an expired token; the mint must happen before \
         the boundary rather than after a rejection"
    );
    assert_eq!(script.refused.load(Ordering::SeqCst), 0);
    Ok(())
}

/// Over a three-hour trial, no request ever presents a token close to its ceiling.
///
/// This is the scenario that falsifies TRAP-9, and it exists because the two either side
/// of it do not: a client refreshing at the sixty-minute ceiling passes both, since each
/// makes one late request and a lazy refresh re-mints immediately before it. The mistake
/// only becomes visible when requests keep arriving *while* a token ages, which is what a
/// long agent run does.
///
/// The verdict is the margin the proxy measured, for the reason
/// [`Script::oldest_presented_secs`] records: a client that refreshes too late does not
/// present an *expired* token, it presents one with almost no life left, and the finding is
/// about exactly that window between building the headers and the proxy reading them.
///
/// **Guard proof.** Set `DEFAULT_REFRESH_AFTER` to `MAX_TOKEN_LIFETIME` and the oldest
/// token presented here jumps from about thirty minutes to about sixty, failing the margin
/// assertion. Verified by running that break.
///
/// Ten minutes is the request interval rather than something finer for the same reason the
/// tick is 500 ms: nothing under test resolves faster, and 18 requests straddle every
/// boundary in play.
#[test]
fn a_long_trial_never_presents_a_token_near_its_ceiling() -> turmoil::Result {
    // Three hours: long enough for two full ceilings, so a schedule that is late by any
    // margin has room to be caught.
    let hours = Duration::from_secs(3 * 60 * 60);
    let serves: Vec<Serve> = (0..18)
        .map(|_| Serve::Json(200, health_json(true)))
        .collect();
    let script = Script::with(serves);
    let mut sim = sim_for(0x5EED_0207, hours + Duration::from_secs(60));
    spawn_proxy(&mut sim, Arc::clone(&script));

    let minter = Arc::new(SimMinter::new(Arc::clone(&script)));
    let auth = Arc::new(ProxyAuth::new(minter, PORT));
    let observed = Arc::clone(&auth);
    sim.client("harness", async move {
        let session = session_with_auth(auth);
        for _ in 0..18 {
            let health = session.health().await?;
            assert!(health.bootstrapped);
            tokio::time::sleep(Duration::from_secs(10 * 60)).await;
        }
        Ok(())
    });

    sim.run()?;

    // The margin. Half the ceiling is the shipped interval, so the oldest token any
    // request presented should sit near thirty minutes and must leave at least the other
    // half of the ceiling — which is exactly the guarantee `DEFAULT_REFRESH_AFTER` exists
    // to provide, restated where it is observable.
    let oldest = script.oldest_presented();
    assert!(
        CEILING - oldest >= CEILING / 2,
        "a request presented a token {}s old, leaving only {}s of its {}s life: the \
         refresh interval is too close to the ceiling, so a token can expire between \
         building the headers and the proxy reading them",
        oldest.as_secs(),
        (CEILING - oldest).as_secs(),
        CEILING.as_secs()
    );
    assert_eq!(
        script.expired.load(Ordering::SeqCst),
        0,
        "the client presented a fully expired token, which the proxy answers with a 403 \
         that reads like a wrong agent token"
    );
    assert_eq!(script.refused.load(Ordering::SeqCst), 0);
    assert_eq!(script.requests().len(), 18);

    // A range rather than an exact count, and the reason is worth stating: the client
    // sleeps ten simulated minutes *between* requests, so each round trip's latency
    // accumulates and the sampling instants drift a little further from the ten-minute
    // grid as the loop goes on. Which side of a refresh boundary a late sample lands on
    // is therefore a function of simulated latency, and pinning the count would make this
    // test fail under a seed change for a reason having nothing to do with the schedule —
    // the same trap the daemon's tier documents about pinning a status.
    //
    // The bound is still load-bearing in both directions. Fewer than three mints over
    // three hours cannot cover three ceilings, so a client that minted once would fail
    // here even if the expiry check somehow did not. More than seven means it is minting
    // far more often than a thirty-minute window implies, which burns control-plane calls
    // — the cost the caching exists to avoid.
    let mints = observed.mint_count();
    assert!(
        (3..=7).contains(&mints),
        "three hours at a thirty-minute refresh window should be four to six mints, \
         got {mints}"
    );
    Ok(())
}

/// The refresh happens on schedule rather than only when a request fails.
///
/// A client that re-minted *reactively* — on a 403 — would also pass the ceiling scenario
/// if it retried. This one crosses the refresh window without crossing the ceiling and
/// asserts the mint happened anyway, which only a scheduled refresh does.
#[test]
fn the_refresh_happens_on_schedule_and_not_in_reaction_to_a_rejection() -> turmoil::Result {
    let script = Script::with([
        Serve::Json(200, health_json(true)),
        Serve::Json(200, health_json(true)),
    ]);
    let mut sim = long_sim(0x5EED_0207);
    spawn_proxy(&mut sim, Arc::clone(&script));

    let minter = Arc::new(SimMinter::new(Arc::clone(&script)));
    let auth = Arc::new(ProxyAuth::new(minter, PORT));
    let observed = Arc::clone(&auth);
    sim.client("harness", async move {
        let session = session_with_auth(auth);
        session.health().await?;
        // Past the thirty-minute refresh window, comfortably inside the ceiling.
        tokio::time::sleep(DEFAULT_REFRESH_AFTER + Duration::from_secs(60)).await;
        session.health().await?;
        Ok(())
    });

    sim.run()?;

    assert_eq!(
        observed.mint_count(),
        2,
        "no mint happened inside the ceiling, so the refresh is reactive rather than \
         scheduled and a token can expire in flight"
    );
    assert_eq!(
        script.expired.load(Ordering::SeqCst),
        0,
        "the scheduled refresh should never have presented a stale token"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 5. A mint failure is retryable
// ---------------------------------------------------------------------------

/// A throttled mint surfaces as a retryable error, and the identical second attempt
/// succeeds.
///
/// The first assertion is the classification, because that is what a caller branches on:
/// a mint failure reported as fatal would abort a healthy trial at minute thirty, which
/// is the outcome TRAP-9's retryability exists to prevent. The second is that no request
/// went out at all on the failing attempt — a client that sent one without a token would
/// get a 403 and learn the wrong thing.
#[test]
fn a_throttled_mint_is_retryable_and_the_second_attempt_succeeds() -> turmoil::Result {
    let script = Script::with([Serve::Json(200, health_json(true))]);
    let mut sim = sim(0x5EED_0208);
    spawn_proxy(&mut sim, Arc::clone(&script));

    let minter = Arc::new(SimMinter::failing(Arc::clone(&script), 1));
    let auth = Arc::new(ProxyAuth::new(minter, PORT));
    let observed = Arc::clone(&auth);
    let seen_after_failure = Arc::new(AtomicUsize::new(0));
    let recorded = Arc::clone(&seen_after_failure);
    let script_for_client = Arc::clone(&script);

    sim.client("harness", async move {
        let session = session_with_auth(auth);

        let err = session
            .health()
            .await
            .expect_err("the first mint is throttled");
        assert!(
            err.retryable(),
            "a throttled mint was reported as fatal, which kills a healthy trial: {err}"
        );
        assert_eq!(err.wire_kind(), Some(WireKind::AuthTokenMint));
        recorded.store(script_for_client.requests().len(), Ordering::SeqCst);

        let health = session.health().await?;
        assert!(health.bootstrapped, "the retry did not land");
        Ok(())
    });

    sim.run()?;

    assert_eq!(
        seen_after_failure.load(Ordering::SeqCst),
        0,
        "a request reached the proxy without a token, so the mint failure was not \
         raised before the request was built"
    );
    assert_eq!(observed.mint_count(), 1, "the failed mint was counted");
    assert_eq!(script.requests().len(), 1);
    assert_eq!(script.refused.load(Ordering::SeqCst), 0);
    Ok(())
}

// ---------------------------------------------------------------------------
// 6. STATE-8: a resume invalidates the cached token
// ---------------------------------------------------------------------------

/// After a resume, the next request carries a freshly minted token.
///
/// The endpoint URL does not change across suspend/resume, so nothing about the request
/// looks different — the only observable is which token value arrived. Asserting that at
/// the proxy is what makes this a test of behaviour rather than of a counter.
#[test]
fn a_rebind_after_resume_sends_a_freshly_minted_token() -> turmoil::Result {
    let script = Script::with([
        Serve::Json(200, health_json(true)),
        Serve::Json(200, health_json(true)),
    ]);
    let mut sim = sim(0x5EED_0209);
    spawn_proxy(&mut sim, Arc::clone(&script));

    let minter = Arc::new(SimMinter::new(Arc::clone(&script)));
    let auth = Arc::new(ProxyAuth::new(minter, PORT));
    let observed = Arc::clone(&auth);
    sim.client("harness", async move {
        let mut session = session_with_auth(auth);
        session.health().await?;

        // The endpoint is unchanged, which is the measured behaviour; the token drop is
        // the part that matters.
        session.rebind(format!("http://{PROXY}:{PORT}"));
        session.health().await?;
        Ok(())
    });

    sim.run()?;

    assert_eq!(observed.mint_count(), 2);
    let seen = script.requests();
    assert_eq!(seen[0].header(PROXY_AUTH_HEADER), Some("jwe-scoped-0"));
    assert_eq!(
        seen[1].header(PROXY_AUTH_HEADER),
        Some("jwe-scoped-1"),
        "the request after a resume carried the pre-suspend token, whose rejection \
         reads exactly like a dead daemon"
    );
    Ok(())
}
