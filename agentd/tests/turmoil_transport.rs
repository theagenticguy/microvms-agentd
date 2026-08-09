// SPDX-License-Identifier: Apache-2.0
//! Transport-fault tier: the HTTP layer under a deterministic network.
//!
//! Roughly a quarter of the Python predecessor's defects lived below the router —
//! keep-alive body desync, a connection dropped instead of an error status, a
//! 256 MB body buffered before authorization was consulted, a non-ASCII header
//! that killed the handler thread. None of those are reachable from a handler
//! test, because a handler test hands the router a `Request` that is already
//! framed correctly. They are only reachable by writing bytes at a socket.
//!
//! So this tier drives the real `routes::app` through the real `serve::serve`
//! over a simulated network, and every scenario is pinned to a seed. `serve()` is
//! already generic over `axum::serve::Listener`, which is why the only new code
//! here is the [`SimListener`] newtype: production never links the simulator.
//!
//! Two properties of turmoil make this tier cheap enough to keep in the default
//! suite:
//!
//! * Time is virtual and paused. The 60-minute keep-alive scenario — which exists
//!   because the endpoint's proxy tokens expire at 60 minutes, so a long trial
//!   crosses that boundary on a pooled connection — costs no wall clock.
//! * Every host is one current-thread runtime stepped by hand, so "the daemon
//!   survived" is a checkable fact rather than a hope: if a connection task
//!   panicked or the accept loop died, the host's software errors and `sim.run()`
//!   fails the test.
//!
//! # Two things a future maintainer will otherwise rediscover the hard way
//!
//! **Rejection closes the connection abortively, without `Connection: close`.**
//! When the daemon stops draining, hyper does not negotiate a shutdown — it drops
//! the connection, and turmoil turns a drop with unread bytes into a RST. So the
//! observable is never a header: it is whether a *subsequent request on the same
//! socket* is answered. Every drain-or-close assertion below is written that way,
//! via [`still_usable`]. An assertion on `Connection: close` looks equivalent and
//! silently never fires.
//!
//! **Whether the RST beats the 401 to the client is a race the seed decides.**
//! Across seeds, a rejected 256 KiB body sometimes yields a readable 401 and
//! sometimes a reset mid-read. Both are the same daemon decision, so the tests
//! classify that into [`Rejection`] and assert on the connection's fate, which is
//! stable. Pinning the status alone would make the tier flaky under a seed change
//! for a reason having nothing to do with the daemon.
//!
//! **An SSE stream has to be read off a raw socket, not through [`Client`].**
//! `Client::send` collects the whole body, which for a stream means it returns
//! once the exec is over — and every property worth checking about streaming is
//! only observable partway through. So [`SseSocket`] de-chunks and frames the body
//! incrementally. Two things it exposes deliberately: the raw decoded body, because
//! a keep-alive is a comment frame with no event name and would otherwise be
//! discarded by the parser, and whether the body ended with the terminating
//! zero-length chunk, because "the command exited" and "the connection died" are
//! the same event count and differ only in that.
//!
//! **There are two clocks, and an exec's child obeys the wrong one.** Everything
//! inside the simulation — a client's `sleep`, the daemon's `sse_keepalive`, its
//! `output_linger` and `stdin_write_timeout` — runs on turmoil's paused virtual
//! clock, which advances one `tick_duration` per `sim.step()` and therefore leaps
//! seconds ahead per millisecond of wall time. A spawned child does not: `sleep 2`
//! in `/bin/sh` is two *real* seconds. Measured here, 2 real seconds of child sleep
//! elapsed while the simulation advanced 30 virtual ones.
//!
//! Two consequences, both of which cost a debugging round:
//!
//! * A child paced by `sleep` is unsynchronized with anything a test asserts. A
//!   client that waits "4 seconds" for the second half of a command's output waits
//!   a few real milliseconds and sees nothing.
//! * Worse, `output_linger` is a virtual deadline measured against a real drain, so
//!   the default 5 seconds expires almost immediately in wall-clock terms. The
//!   waiter then abandons the pipes with the child still writing, and the exec
//!   reports `writers_may_be_alive` with output silently missing — a daemon-side
//!   truncation the simulator caused.
//!
//! So no scenario below paces a child with `sleep`. Children that must block do it
//! on `read`, which [`write_stdin`] releases, making the harness the clock: the
//! ordering a scenario needs is caused rather than hoped for, and it holds under any
//! tick duration. Where a child cannot be paced that way, [`patient`] raises
//! `output_linger` far past the scenario so the virtual deadline cannot cut a real
//! drain short.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agentd::{AppState, Config, routes, serve};
use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use turmoil::Sim;
use turmoil::net::{TcpListener, TcpStream};

/// The port the daemon listens on inside the simulation. Matches the production
/// default so nothing about the routing under test is special-cased.
const PORT: u16 = 9000;

/// The platform's run-hook path. Fixed by the service, so a test that uses a
/// bare `/run` would pass against a daemon the platform cannot bootstrap.
const RUN_HOOK: &str = "/aws/lambda-microvms/runtime/v1/run";

/// The token the platform's run hook installs. Long enough that a truncating
/// comparison would not accidentally match.
const TOKEN: &str = "sim-agent-token-0123456789";

/// The daemon host's name in simulated DNS.
const DAEMON: &str = "agentd";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Adapts a turmoil listener to `axum::serve::Listener`, counting accepts.
///
/// The count is the load-bearing part. Connection reuse is invisible in a
/// response body, so a test that only checks statuses cannot tell a keep-alive
/// server from one that closes after every response — and "close after every
/// response" is the shape a daemon degrades into once its body framing is wrong.
struct SimListener {
    inner: TcpListener,
    accepted: Arc<AtomicUsize>,
}

impl axum::serve::Listener for SimListener {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.inner.accept().await {
                Ok(pair) => {
                    self.accepted.fetch_add(1, Ordering::Relaxed);
                    return pair;
                }
                // The trait contract is that `accept` never returns an error, so
                // a failed accept must not end the loop: a daemon that stops
                // accepting is indistinguishable from a dead VM, and the
                // platform has no supervisor inside it to notice.
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// Registers the daemon as a simulated host and returns its accept counter.
///
/// The `AppState` is created once outside the host factory, so bootstrap state
/// survives a restart the way it survives a reconnect in production.
fn spawn_daemon(sim: &mut Sim<'_>, config: Config) -> Arc<AtomicUsize> {
    let state = AppState::new(config);
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);

    sim.host(DAEMON, move || {
        let state = state.clone();
        let accepted = Arc::clone(&counter);
        async move {
            let inner = TcpListener::bind((Ipv4Addr::UNSPECIFIED, PORT)).await?;
            let listener = SimListener { inner, accepted };
            serve::serve(listener, routes::app(state)).await?;
            Ok(())
        }
    });

    accepted
}

/// A simulation seeded for reproducibility.
///
/// The duration is raised off the 10-second default because several scenarios
/// deliberately wait on a connection that should be dead: the wait is virtual, but
/// it still has to fit inside the simulation or the sim errors out before the
/// assertion runs, which reads as a harness failure rather than a daemon one.
fn sim(seed: u64) -> Sim<'static> {
    turmoil::Builder::new()
        .rng_seed(seed)
        // The production `serve()` listens for SIGTERM so the platform's
        // `/terminate` can drain in-flight requests, and registering a signal
        // handler needs tokio's I/O driver, which turmoil leaves off by default.
        // Enabling it does not let network I/O escape the simulation, because
        // `turmoil::net` never touches that driver.
        .enable_tokio_io()
        .simulation_duration(Duration::from_secs(120))
        .build()
}

/// A simulation whose virtual clock has room for the proxy-token boundary.
///
/// The tick is coarsened deliberately. Stepping 70 simulated minutes at the
/// default 1 ms tick is 4.2 million steps; at 500 ms it is 8,400. Nothing under
/// test resolves faster than a network round trip, so the coarser tick costs no
/// fidelity.
fn long_sim(seed: u64) -> Sim<'static> {
    turmoil::Builder::new()
        .rng_seed(seed)
        .enable_tokio_io()
        .simulation_duration(Duration::from_secs(90 * 60))
        .tick_duration(Duration::from_millis(500))
        .build()
}

/// Caps sized for the tests rather than for a 512 MiB VM, so a scenario can cross
/// a boundary without moving megabytes through the simulated network.
fn config(max_body_bytes: usize, max_drain_bytes: usize) -> Config {
    Config {
        port: PORT,
        max_body_bytes,
        max_drain_bytes,
        ..Config::default()
    }
}

/// Production defaults with the streaming and stdin knobs overridden.
///
/// A sibling of [`config`] rather than more parameters on it: the streaming
/// scenarios vary one knob each and would otherwise pass four defaults
/// positionally, where a transposed pair is silent. The body and drain caps stay
/// at production values here so a streaming assertion cannot be an artifact of a
/// cap the scenario did not mean to move.
fn tuned(f: impl FnOnce(&mut Config)) -> Config {
    let mut cfg = Config {
        port: PORT,
        ..Config::default()
    };
    f(&mut cfg);
    cfg
}

/// [`tuned`] with the post-exit drain deadline pushed past any scenario.
///
/// Necessary rather than tidy: `output_linger` is a virtual deadline and a child's
/// pipe drain is real work, so the production 5 seconds expires in a few real
/// milliseconds here and the waiter abandons a child that is still writing. See the
/// module docs on the two clocks. Raising it means a streaming assertion measures
/// the daemon's framing rather than the simulator's clock skew.
fn patient(f: impl FnOnce(&mut Config)) -> Config {
    tuned(|cfg| {
        cfg.output_linger = Duration::from_secs(3600);
        f(cfg);
    })
}

/// One HTTP/1.1 connection driven by hyper, with the driver task detached.
struct Client {
    sender: http1::SendRequest<Full<Bytes>>,
}

impl Client {
    /// Opens a connection and spawns its driver.
    ///
    /// The `SendRequest` handle is what makes keep-alive reuse structural: every
    /// request issued through it goes down the same socket, so the accept counter
    /// on the server measures whether the server agreed to reuse it.
    async fn connect() -> turmoil::Result<Self> {
        let stream = TcpStream::connect((DAEMON, PORT)).await?;
        let (sender, conn) = http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(async move {
            // A connection error here is the subject of several scenarios, so it
            // is swallowed rather than propagated: the assertion belongs to the
            // request that observed it.
            let _ = conn.await;
        });
        Ok(Self { sender })
    }

    async fn send(
        &mut self,
        request: Request<Full<Bytes>>,
    ) -> hyper::Result<(StatusCode, Vec<u8>)> {
        let response = self.sender.send_request(request).await?;
        let status = response.status();
        let body = response.into_body().collect().await?.to_bytes().to_vec();
        Ok((status, body))
    }
}

/// A `POST` to the platform's run hook carrying `token`.
///
/// The double encoding is the platform's, not ours: the `runHookPayload` string
/// passed to `RunMicrovm` arrives wrapped as
/// `{"runHookPayload": "<that string>"}`. Building the body any other way makes
/// this whole tier pass against a daemon the platform cannot bootstrap, which is
/// exactly what happened before a live run reported
/// "Run lifecycle hook returned HTTP status 400".
fn run_hook(token: &str) -> Request<Full<Bytes>> {
    let inner = format!(r#"{{"agent_token":"{token}"}}"#);
    let body = serde_json::json!({ "runHookPayload": inner }).to_string();
    Request::builder()
        .method("POST")
        .uri(RUN_HOOK)
        .header("host", "localhost")
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("well-formed run hook")
}

/// A `GET /v1/health`, which needs no token and reports bootstrap state.
fn health() -> Request<Full<Bytes>> {
    Request::builder()
        .method("GET")
        .uri("/v1/health")
        .header("host", "localhost")
        .body(Full::new(Bytes::new()))
        .expect("well-formed health request")
}

/// Bootstraps the daemon over a throwaway connection.
async fn bootstrap() -> turmoil::Result<()> {
    let mut client = Client::connect().await?;
    let (status, _) = client.send(run_hook(TOKEN)).await?;
    assert_eq!(status, StatusCode::OK, "bootstrap must succeed");
    Ok(())
}

// ---------------------------------------------------------------------------
// Raw-socket helpers
// ---------------------------------------------------------------------------

/// A response parsed off the wire without hyper's help.
///
/// Several scenarios have to send bytes hyper's client would refuse to produce —
/// a `Content-Length` that overstates what follows, a body that stops early, a
/// TLS ClientHello — so they read the reply the same way.
#[derive(Debug)]
struct Raw {
    status: u16,
}

/// Reads one response head plus its declared body off `stream`.
///
/// The body is consumed so the connection is left at a message boundary;
/// otherwise the next read in a keep-alive scenario would see this response's
/// leftovers and report a desync the harness caused itself.
async fn read_raw(stream: &mut TcpStream) -> io::Result<Raw> {
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
                "connection closed before a complete response head",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unparseable status line {status_line:?}"),
            )
        })?;

    let declared = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);

    let mut have = buf.len() - head_end;
    while have < declared {
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        have += n;
    }

    Ok(Raw { status })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Whether `stream` can still carry another request.
///
/// This is the only reliable way to observe the drain-or-close decision — see the
/// module docs on why `Connection: close` is not it. The timeout is a safety net
/// so a daemon that neither answers nor closes fails as this assertion rather
/// than as an exhausted simulation.
async fn still_usable(stream: &mut TcpStream) -> bool {
    if stream
        .write_all(b"GET /v1/health HTTP/1.1\r\nhost: localhost\r\n\r\n")
        .await
        .is_err()
    {
        return false;
    }
    if stream.flush().await.is_err() {
        return false;
    }
    matches!(
        tokio::time::timeout(Duration::from_secs(5), read_raw(stream)).await,
        Ok(Ok(raw)) if raw.status == 200
    )
}

/// Formats a request head with a `Content-Length` the caller controls, including
/// one that overstates what the caller intends to send.
fn head_with_length(method: &str, path: &str, token: Option<&str>, length: usize) -> Vec<u8> {
    let mut head = format!("{method} {path} HTTP/1.1\r\nhost: localhost\r\n");
    if let Some(token) = token {
        head.push_str(&format!("authorization: Bearer {token}\r\n"));
    }
    head.push_str(&format!("content-length: {length}\r\n\r\n"));
    head.into_bytes()
}

/// What the daemon did with a rejected request.
#[derive(Debug, Eq, PartialEq)]
enum Rejection {
    /// A status arrived on the wire.
    Answered(u16),
    /// The daemon closed the connection before a status could be read. Still a
    /// refusal, and the client learns the request did not happen — it just learns
    /// it as a transport error instead of a status.
    Closed,
}

/// Sends an unauthorized request whose body is `body_len` bytes, delivered in
/// full, then reports the refusal and whether the connection survived it.
async fn unauthorized_upload(body_len: usize) -> io::Result<(Rejection, bool)> {
    let mut stream = TcpStream::connect((DAEMON, PORT)).await?;
    let request = head_with_length(
        "PUT",
        "/v1/fs/file?path=/tmp/never-written",
        Some("wrong-token"),
        body_len,
    );
    // A write failure is itself the daemon having closed on us, which is the same
    // refusal this helper reports; it must not abort the test.
    if stream.write_all(&request).await.is_err() {
        return Ok((Rejection::Closed, false));
    }
    if stream.write_all(&vec![b'x'; body_len]).await.is_err() {
        return Ok((Rejection::Closed, false));
    }
    let _ = stream.flush().await;

    let rejection = match tokio::time::timeout(Duration::from_secs(10), read_raw(&mut stream)).await
    {
        Ok(Ok(raw)) => Rejection::Answered(raw.status),
        Ok(Err(_)) => Rejection::Closed,
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the daemon neither answered the rejected request nor closed",
            ));
        }
    };
    let usable = match rejection {
        Rejection::Answered(_) => still_usable(&mut stream).await,
        Rejection::Closed => false,
    };
    Ok((rejection, usable))
}

// ---------------------------------------------------------------------------
// Exec control helpers
// ---------------------------------------------------------------------------

/// A request carrying the bootstrapped token, with a JSON body when there is one.
fn authorized(method: &str, uri: &str, body: serde_json::Value) -> Request<Full<Bytes>> {
    let body = if body.is_null() {
        String::new()
    } else {
        body.to_string()
    };
    Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "localhost")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("well-formed authorized request")
}

/// Starts an exec over the simulated network and asserts the daemon took it.
async fn start_exec(id: &str, script: &str, stdin: bool) -> turmoil::Result<()> {
    let mut client = Client::connect().await?;
    let (status, body) = client
        .send(authorized(
            "POST",
            "/v1/exec/start",
            serde_json::json!({
                "exec_id": id,
                "command": ["/bin/sh", "-c", script],
                "stdin": stdin,
            }),
        ))
        .await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "start {id} refused: {}",
        String::from_utf8_lossy(&body)
    );
    Ok(())
}

/// Polls an exec on a brand-new connection.
///
/// New every call on purpose: every scenario below has just abandoned a
/// connection, and reusing a pooled one would make "the exec survived" partly a
/// statement about that socket rather than about the server-side object.
async fn poll_exec(id: &str) -> turmoil::Result<serde_json::Value> {
    let mut client = Client::connect().await?;
    let (status, body) = client
        .send(authorized("GET", &format!("/v1/exec/{id}"), json_none()))
        .await?;
    assert_eq!(status, StatusCode::OK, "poll {id} was not answered");
    Ok(serde_json::from_slice(&body)?)
}

/// Polls until the exec leaves `running`, so a scenario asserts on a finished
/// exec rather than on whatever phase the sim happened to be in.
///
/// The bound is on attempts rather than on virtual time: virtual time is free, so
/// a wall-clock deadline here would either be enormous or would depend on how fast
/// the sim steps.
async fn await_exit(id: &str) -> turmoil::Result<serde_json::Value> {
    for _ in 0..4_000 {
        let polled = poll_exec(id).await?;
        if polled["phase"] != "running" {
            return Ok(polled);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("exec {id} never left the running phase");
}

/// Writes to an exec's stdin, returning the status and the parsed body.
async fn write_stdin(
    id: &str,
    data: Option<&[u8]>,
    eof: bool,
) -> turmoil::Result<(StatusCode, serde_json::Value)> {
    let mut client = Client::connect().await?;
    let (status, body) = client
        .send(authorized(
            "POST",
            &format!("/v1/exec/{id}/stdin"),
            stdin_body(data, eof),
        ))
        .await?;
    Ok((status, serde_json::from_slice(&body).unwrap_or_default()))
}

/// A stdin request body. Base64 because stdin is bytes and a JSON string cannot
/// hold arbitrary ones.
fn stdin_body(data: Option<&[u8]>, eof: bool) -> serde_json::Value {
    let mut body = serde_json::Map::new();
    if let Some(data) = data {
        body.insert(
            "data_b64".to_string(),
            serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(data)),
        );
    }
    if eof {
        body.insert(
            "signal".to_string(),
            serde_json::Value::String("eof".to_string()),
        );
    }
    serde_json::Value::Object(body)
}

/// A `null` body, i.e. no body at all. Named so a `GET` reads as one.
fn json_none() -> serde_json::Value {
    serde_json::Value::Null
}

// ---------------------------------------------------------------------------
// SSE over a raw socket
// ---------------------------------------------------------------------------

/// One parsed SSE event: its `event:` name and its `data:` JSON.
#[derive(Debug)]
struct Sse {
    name: String,
    data: serde_json::Value,
}

impl Sse {
    /// The decoded bytes of an `output` event.
    fn output(&self) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(self.data["output"].as_str().expect("output is a string"))
            .expect("output is base64")
    }

    fn offset(&self) -> u64 {
        self.data["offset"].as_u64().expect("offset is a number")
    }

    fn field(&self, key: &str) -> u64 {
        self.data[key]
            .as_u64()
            .unwrap_or_else(|| panic!("{key} is a number in {:?}", self.data))
    }
}

/// An SSE attach read incrementally off a turmoil socket.
///
/// Hand-rolled rather than driven by hyper because the scenarios need to stop
/// reading and abandon the socket mid-stream, and a pooled client abstracts away
/// exactly the moment that matters. Chunked framing is decoded here too: axum
/// answers a stream with `transfer-encoding: chunked` and no length, so the SSE
/// frames arrive interleaved with chunk headers.
struct SseSocket {
    stream: TcpStream,
    /// Undecoded chunked-transfer bytes.
    wire: Vec<u8>,
    /// Decoded body, minus whatever [`Self::next`] has already framed.
    pending: String,
    /// Bytes still owed to the chunk currently being decoded.
    chunk_left: usize,
    ended: bool,
    /// Whether the body ended with the terminating zero-length chunk rather than
    /// with a FIN or a reset. This is the transport-level half of the same
    /// distinction the `exit` event carries at the protocol level.
    graceful: bool,
    /// Every decoded body byte, kept because keep-alive comments are not events
    /// and are therefore invisible to the parsed stream.
    raw: String,
}

impl SseSocket {
    /// Opens an attach and consumes the response head, asserting on the two
    /// headers a client's framing depends on.
    async fn attach(id: &str, offset: Option<u64>) -> io::Result<Self> {
        let mut stream = TcpStream::connect((DAEMON, PORT)).await?;
        let path = match offset {
            Some(from) => format!("/v1/exec/{id}/stream?offset={from}"),
            None => format!("/v1/exec/{id}/stream"),
        };
        let head = format!(
            "GET {path} HTTP/1.1\r\nhost: localhost\r\n\
             authorization: Bearer {TOKEN}\r\naccept: text/event-stream\r\n\r\n"
        );
        stream.write_all(head.as_bytes()).await?;
        stream.flush().await?;

        let mut wire = Vec::new();
        let head_end = loop {
            if let Some(idx) = find(&wire, b"\r\n\r\n") {
                break idx + 4;
            }
            let mut chunk = [0u8; 4096];
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the attach closed before a response head",
                ));
            }
            wire.extend_from_slice(&chunk[..n]);
        };

        let head = String::from_utf8_lossy(&wire[..head_end]).into_owned();
        let lower = head.to_ascii_lowercase();
        assert!(
            lower.starts_with("http/1.1 200"),
            "the attach was refused: {head}"
        );
        assert!(
            lower.contains("content-type: text/event-stream"),
            "the attach is not framed as SSE, so a client cannot tell exit from \
             disconnect: {head}"
        );
        // Without this a buffering proxy holds events until its own buffer fills,
        // which turns a live stream into one batch delivered at exit.
        assert!(
            lower.contains("x-accel-buffering: no"),
            "the proxy-buffering opt-out is missing: {head}"
        );
        wire.drain(..head_end);

        Ok(Self {
            stream,
            wire,
            pending: String::new(),
            chunk_left: 0,
            ended: false,
            graceful: false,
            raw: String::new(),
        })
    }

    /// Moves whatever chunked bytes have arrived into the decoded body.
    ///
    /// Partial by design: a chunk header split across two reads leaves `wire`
    /// untouched and the caller reads more. Returning early rather than guessing
    /// at a truncated header is what keeps a slow delivery from being misparsed as
    /// a framing defect.
    fn dechunk(&mut self) {
        loop {
            if self.chunk_left == 0 {
                if self.wire.starts_with(b"\r\n") {
                    self.wire.drain(..2);
                    continue;
                }
                let Some(idx) = find(&self.wire, b"\r\n") else {
                    return;
                };
                let header = String::from_utf8_lossy(&self.wire[..idx]).into_owned();
                let size = usize::from_str_radix(header.trim(), 16)
                    .unwrap_or_else(|_| panic!("unparseable chunk size {header:?}"));
                self.wire.drain(..idx + 2);
                if size == 0 {
                    self.ended = true;
                    self.graceful = true;
                    return;
                }
                self.chunk_left = size;
            }
            let take = self.chunk_left.min(self.wire.len());
            if take == 0 {
                return;
            }
            let bytes: Vec<u8> = self.wire.drain(..take).collect();
            let text = String::from_utf8(bytes).expect("SSE bodies are UTF-8");
            self.raw.push_str(&text);
            self.pending.push_str(&text);
            self.chunk_left -= take;
        }
    }

    /// The next named event, skipping keep-alive comments. `None` once the body
    /// ends, however it ended — see [`Self::graceful`] for which.
    async fn next(&mut self) -> io::Result<Option<Sse>> {
        loop {
            self.dechunk();
            if let Some(idx) = self.pending.find("\n\n") {
                let frame = self.pending[..idx].to_string();
                self.pending.drain(..idx + 2);
                if let Some(event) = parse_sse(&frame) {
                    return Ok(Some(event));
                }
                continue;
            }
            if self.ended {
                return Ok(None);
            }
            let mut chunk = [0u8; 4096];
            let n = self.stream.read(&mut chunk).await?;
            if n == 0 {
                // A FIN with no terminating chunk. The body is over but it did not
                // end the way a completed response does.
                self.ended = true;
                return Ok(None);
            }
            self.wire.extend_from_slice(&chunk[..n]);
        }
    }

    /// Reads to the end of the stream, bounded so a regression that never
    /// terminates fails here rather than exhausting the simulation.
    async fn drain(&mut self) -> io::Result<Vec<Sse>> {
        let mut seen = Vec::new();
        for _ in 0..20_000 {
            match self.next().await? {
                Some(event) => seen.push(event),
                None => return Ok(seen),
            }
        }
        panic!("the stream never ended");
    }

    /// How many keep-alive comment frames the raw body carries.
    ///
    /// Counted off the raw bytes rather than the parsed events because a
    /// keep-alive has no `event:` line at all: it is `:\n\n`, which the framing
    /// parser correctly discards.
    fn keepalives(&self) -> usize {
        self.raw.matches(":\n\n").count()
    }
}

fn parse_sse(frame: &str) -> Option<Sse> {
    let mut name = None;
    let mut data = String::new();
    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("event: ") {
            name = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("data: ") {
            data.push_str(rest);
        }
    }
    // A keep-alive is a bare comment: no name, no data.
    let name = name?;
    Some(Sse {
        name,
        data: serde_json::from_str(&data).expect("event data is JSON"),
    })
}

/// What one attach accounted for: bytes handed over, bytes honestly declared
/// lost, and the cursor the terminal event agreed with.
#[derive(Debug, Default, Eq, PartialEq)]
struct Ledger {
    delivered: u64,
    skipped: u64,
    bytes: Vec<u8>,
}

/// Walks an attach's events as a tiling of the offset space starting at `from`.
///
/// This is the assertion that makes "no silent loss" checkable rather than
/// hopeful. Every byte between `from` and the terminal offset must be covered
/// exactly once by either an `output` event or a `gap` — a discontinuity is a
/// stream a client cannot reconcile, and an overlap is duplicated output. Silent
/// loss is precisely the case where the events tile a *smaller* range than the
/// terminal offset names, which is why the total is checked against `exit`.
fn tile(events: &[Sse], from: u64) -> Ledger {
    let mut ledger = Ledger::default();
    let mut cursor = from;
    let mut terminal = None;

    for event in events {
        match event.name.as_str() {
            "output" => {
                assert!(
                    terminal.is_none(),
                    "an output event followed the terminal one: {events:#?}"
                );
                let bytes = event.output();
                assert_eq!(
                    event.offset(),
                    cursor,
                    "offsets are discontinuous, so a client cannot resume: {events:#?}"
                );
                cursor += bytes.len() as u64;
                ledger.delivered += bytes.len() as u64;
                ledger.bytes.extend_from_slice(&bytes);
            }
            "gap" => {
                let lost_from = event.field("from");
                let lost_to = event.field("to");
                assert_eq!(
                    lost_from, cursor,
                    "a gap must start where the last byte left off: {events:#?}"
                );
                assert!(lost_to > lost_from, "an empty gap reports nothing");
                cursor = lost_to;
                ledger.skipped += lost_to - lost_from;
            }
            "exit" => {
                assert!(terminal.is_none(), "two terminal events: {events:#?}");
                terminal = Some(event.offset());
            }
            other => panic!("unknown event {other:?}: {events:#?}"),
        }
    }

    let total = terminal.unwrap_or_else(|| panic!("no terminal event: {events:#?}"));
    assert_eq!(
        cursor, total,
        "the events account for {cursor} bytes but the exit event names {total}: \
         the difference is output lost without a gap to say so"
    );
    ledger
}

/// The terminal event of a stream, or `None` if it never arrived.
fn exit_of(events: &[Sse]) -> Option<&Sse> {
    let last = events.last()?;
    (last.name == "exit").then_some(last)
}

// ---------------------------------------------------------------------------
// 1. The harness is real
// ---------------------------------------------------------------------------

/// Bootstrap and an authorized control request, over the simulated network.
///
/// This exists to keep every other test in this file honest. If the listener
/// adapter, the DNS name, or the port were wrong, the failures below would all
/// look like transport defects in the daemon.
#[test]
fn a_bootstrapped_daemon_serves_an_authorized_control_request() -> turmoil::Result {
    let mut sim = sim(0x5EED_0001);
    spawn_daemon(&mut sim, Config::default());

    sim.client("harness", async {
        let mut client = Client::connect().await?;

        let (status, _) = client.send(run_hook(TOKEN)).await?;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = client.send(health()).await?;
        assert_eq!(status, StatusCode::OK);
        let health: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(health["bootstrapped"], serde_json::Value::Bool(true));

        // An authorized control request reaches its handler. The 404 is the
        // handler's own answer for an exec id that does not exist, and its body
        // proves the request was routed rather than rejected — 401 and 503 both
        // carry no body at all.
        let authorized = Request::builder()
            .method("GET")
            .uri("/v1/exec/never-started")
            .header("host", "localhost")
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(Full::new(Bytes::new()))?;
        let (status, body) = client.send(authorized).await?;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let error: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(error["error"], "unknown_exec");

        // The same request without the token is refused, so the 404 above is a
        // property of authorization succeeding and not of the guard being absent.
        let unauthorized = Request::builder()
            .method("GET")
            .uri("/v1/exec/never-started")
            .header("host", "localhost")
            .body(Full::new(Bytes::new()))?;
        let (status, _) = client.send(unauthorized).await?;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        Ok(())
    });

    sim.run()
}

/// An unbootstrapped daemon answers 503, and only a bad credential gets 401.
///
/// These two codes carry different instructions to a client: 503 means the VM is
/// not ready and the request is worth retrying, while 401 means the token is wrong
/// and retrying it never helps. A harness that conflates them either gives up on a
/// VM that was still starting or retries a credential failure forever. Neither
/// code may ever be 404, because clients map that onto a missing file and would
/// report a phantom absent artifact for what is really a protocol state.
///
/// This test exists because sabotaging the distinction in `auth.rs` failed nothing
/// in the suite — the property was only covered by a manual smoke run.
#[test]
fn an_unbootstrapped_daemon_answers_503_and_a_bad_token_answers_401() -> turmoil::Result {
    let mut sim = sim(0x5EED_0009);
    spawn_daemon(&mut sim, Config::default());

    sim.client("harness", async {
        let mut client = Client::connect().await?;

        let control = |token: Option<&str>| {
            let mut builder = Request::builder()
                .method("GET")
                .uri("/v1/exec/never-started")
                .header("host", "localhost");
            if let Some(token) = token {
                builder = builder.header("authorization", format!("Bearer {token}"));
            }
            builder.body(Full::new(Bytes::new()))
        };

        // Before bootstrap the control API is closed, whatever the caller
        // presents. A wrong token here is still 503: the daemon has nothing to
        // compare against, so calling it a credential failure would be a guess.
        let (status, _) = client.send(control(None)?).await?;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "no token before bootstrap must be 503, not 401"
        );
        let (status, _) = client.send(control(Some("anything"))?).await?;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "any token before bootstrap must be 503, not 401"
        );

        let (status, _) = client.send(run_hook(TOKEN)).await?;
        assert_eq!(status, StatusCode::OK);

        // After bootstrap the same two requests become credential decisions.
        let (status, _) = client.send(control(None)?).await?;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a missing token after bootstrap must be 401, not 503"
        );
        let (status, _) = client.send(control(Some("wrong"))?).await?;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a wrong token after bootstrap must be 401, not 503"
        );

        Ok(())
    });

    sim.run()
}

// ---------------------------------------------------------------------------
// 2. Keep-alive reuse without body desync
// ---------------------------------------------------------------------------

/// Three requests with bodies on one connection, each answered on its own terms.
///
/// This is the body-desync class. The predecessor left unconsumed request-body
/// bytes in the socket, so the next request on a reused connection began parsing
/// mid-body: the daemon then answered request N+1 with a status derived from
/// request N's leftovers. The statuses here are chosen so that outcome cannot
/// pass — a bootstrap replay only conflicts if its own body arrived intact and
/// was attributed to its own request.
#[test]
fn keep_alive_reuse_does_not_desync_bodies() -> turmoil::Result {
    let mut sim = sim(0x5EED_0002);
    let accepted = spawn_daemon(&mut sim, Config::default());

    sim.client("harness", async {
        let mut client = Client::connect().await?;

        let (status, _) = client.send(run_hook(TOKEN)).await?;
        assert_eq!(status, StatusCode::OK, "first bootstrap installs");

        // A different token on the same connection. 409 is only reachable if this
        // body was parsed as its own message; leftovers from the request above
        // would make it malformed, which is 400.
        let (status, _) = client.send(run_hook("a-different-token")).await?;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "the second body was not attributed to its own request"
        );

        // And the connection is still at a message boundary afterwards.
        let (status, body) = client.send(health()).await?;
        assert_eq!(status, StatusCode::OK);
        let health: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(
            health["bootstrapped"],
            serde_json::Value::Bool(true),
            "the conflicting replay must not have disturbed the installed token"
        );

        Ok(())
    });

    sim.run()?;

    assert_eq!(
        accepted.load(Ordering::Relaxed),
        1,
        "three requests opened more than one connection, so keep-alive is not actually working"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 3. Keep-alive across the proxy-token boundary
// ---------------------------------------------------------------------------

/// A pooled connection survives longer than an endpoint auth token lives.
///
/// The platform caps `X-aws-proxy-auth` at 60 minutes, so any trial longer than
/// an hour mints a fresh token mid-flight and reuses its existing connection to
/// carry the next request. If the daemon aged that connection out, the retry path
/// would meet a transport error at exactly the moment it was recovering from an
/// expiry — the least debuggable moment available.
#[test]
fn a_connection_survives_past_the_proxy_token_lifetime() -> turmoil::Result {
    let mut sim = long_sim(0x5EED_0003);
    let accepted = spawn_daemon(&mut sim, Config::default());

    sim.client("harness", async {
        let mut client = Client::connect().await?;
        let (status, _) = client.send(run_hook(TOKEN)).await?;
        assert_eq!(status, StatusCode::OK);

        // Virtual time, so this is free.
        tokio::time::sleep(Duration::from_secs(70 * 60)).await;

        let (status, body) = client.send(health()).await?;
        assert_eq!(
            status,
            StatusCode::OK,
            "the idle connection did not survive the token boundary"
        );
        let health: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(health["bootstrapped"], serde_json::Value::Bool(true));

        Ok(())
    });

    sim.run()?;

    assert_eq!(
        accepted.load(Ordering::Relaxed),
        1,
        "the client had to reconnect, so the daemon closed an idle connection"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 4. Authorization before the body
// ---------------------------------------------------------------------------

/// A caller declaring 512 MB is refused after the daemon has read kilobytes.
///
/// The predecessor buffered the body and then checked authorization, which let
/// any caller force a 256 MB allocation on a VM whose baseline can be 512 MiB —
/// and an OOM-killed daemon in a MicroVM is unrecoverable, because the platform
/// forwards no traffic to a dead process and nothing inside restarts it.
///
/// The mechanism of the assertion is the ratio: 512 MB is declared, 8 KiB is
/// sent, and the rest never arrives. A daemon that buffered first could not
/// answer at all, so the 401 arriving *is* the property. The second half of the
/// test is the one that makes the first meaningful — the stalled caller does not
/// impair anyone else, which is what distinguishes "refused cheaply" from
/// "refused after tying up the daemon".
#[test]
fn an_unauthorized_request_is_refused_before_its_body_arrives() -> turmoil::Result {
    let mut sim = sim(0x5EED_0004);
    // A wire cap far above the declared length, so `RequestBodyLimitLayer` cannot
    // answer this request on authorization's behalf and the 401 is attributable.
    spawn_daemon(&mut sim, config(1024 * 1024 * 1024, 4096));

    sim.client("harness", async {
        bootstrap().await?;

        const DECLARED: usize = 512 * 1024 * 1024;
        const SENT: usize = 8 * 1024;

        let mut stream = TcpStream::connect((DAEMON, PORT)).await?;
        stream
            .write_all(&head_with_length(
                "PUT",
                "/v1/fs/file?path=/tmp/never-written",
                None,
                DECLARED,
            ))
            .await?;
        stream.write_all(&vec![b'x'; SENT]).await?;
        stream.flush().await?;

        let refused = tokio::time::timeout(Duration::from_secs(30), read_raw(&mut stream)).await;
        let refused = refused.map_err(|_| {
            format!("no answer after {SENT} of {DECLARED} declared bytes: the daemon is waiting on a body it already had grounds to refuse")
        })??;
        assert_eq!(
            refused.status, 401,
            "authorization was not decided before the body"
        );

        // A caller that declares a body and then sends nothing at all holds its
        // own connection open, because the bounded drain is waiting on bytes that
        // never come. That costs one connection and no memory, and the assertion
        // below is what pins the "no memory, no interference" half — if a stalled
        // unauthorized caller could stall the daemon, this is where that shows up.
        let mut stalled = TcpStream::connect((DAEMON, PORT)).await?;
        stalled
            .write_all(&head_with_length(
                "PUT",
                "/v1/fs/file?path=/tmp/never-written",
                None,
                DECLARED,
            ))
            .await?;
        stalled.flush().await?;
        tokio::time::sleep(Duration::from_secs(2)).await;

        let mut bystander = TcpStream::connect((DAEMON, PORT)).await?;
        assert!(
            still_usable(&mut bystander).await,
            "an unauthorized caller sitting on a declared 512 MB body blocked an unrelated connection"
        );

        Ok(())
    });

    sim.run()
}

// ---------------------------------------------------------------------------
// 5. Drain or close
// ---------------------------------------------------------------------------

/// A small rejected body is drained, so a pooled connection keeps working.
///
/// Leaving unread bytes in the receive buffer makes the close abortive, and a
/// pooled client then reports a transport error instead of the 401 the daemon
/// deliberately chose. Draining a bounded prefix is what converts that into a
/// status the client can act on — this is the half of the trade-off that costs
/// something, so it is worth having a test that fails if someone removes it.
#[test]
fn a_small_rejected_body_leaves_the_connection_usable() -> turmoil::Result {
    let mut sim = sim(0x5EED_0005);
    spawn_daemon(&mut sim, config(8 * 1024 * 1024, 1024));

    sim.client("harness", async {
        bootstrap().await?;

        let (rejection, usable) = unauthorized_upload(256).await?;
        assert_eq!(
            rejection,
            Rejection::Answered(401),
            "a body well under the drain cap must still get the status, on the wire"
        );
        assert!(
            usable,
            "the connection was not reusable after a rejection the daemon could have drained"
        );

        Ok(())
    });

    sim.run()
}

/// A rejected body past `max_drain_bytes` costs the caller its connection.
///
/// The other half of the trade-off, and the reason the cap exists: draining
/// without one would let an unauthorized caller keep the daemon reading for as
/// long as it cared to write. Past the cap the daemon stops reading, and because
/// bytes are still inbound the close is abortive — so whether the 401 wins the
/// race to the client varies by seed, and only the connection's fate is asserted.
/// See the module docs.
#[test]
fn a_rejected_body_past_the_drain_cap_closes_the_connection() -> turmoil::Result {
    let mut sim = sim(0x5EED_0006);
    // Same config as the small case above, so the two tests differ only in body
    // size and together isolate the cap rather than any other limit.
    spawn_daemon(&mut sim, config(8 * 1024 * 1024, 1024));

    sim.client("harness", async {
        bootstrap().await?;

        // 256 KiB against a 1 KiB drain cap, delivered in full: this is the cap
        // being exceeded, not the body being truncated, which would end the
        // connection for an unrelated reason.
        let (rejection, usable) = unauthorized_upload(256 * 1024).await?;
        assert!(
            !usable,
            "the daemon kept draining past its cap: a rejected 256 KiB body left the \
             connection reusable, which is the unbounded-drain denial-of-service"
        );
        if let Rejection::Answered(status) = rejection {
            assert_eq!(
                status, 401,
                "when the status does win the race it must be the one authorization chose"
            );
        }

        // The daemon itself is unharmed: only the offending connection was spent.
        let mut fresh = TcpStream::connect((DAEMON, PORT)).await?;
        assert!(
            still_usable(&mut fresh).await,
            "closing the offending connection also took the daemon out"
        );

        Ok(())
    });

    sim.run()
}

// ---------------------------------------------------------------------------
// 6. Mid-body disconnect
// ---------------------------------------------------------------------------

/// A client that vanishes mid-body does not take the daemon with it.
///
/// The predecessor's handler thread died on exactly this, and because it was the
/// listener's thread the VM stopped answering entirely — which the platform
/// reports as an opaque failure, since a dead process gets no traffic and there
/// is no supervisor inside to restart it.
#[test]
fn a_mid_body_disconnect_does_not_take_the_daemon_down() -> turmoil::Result {
    let mut sim = sim(0x5EED_0007);
    let accepted = spawn_daemon(&mut sim, config(1024 * 1024, 64 * 1024));

    sim.client("harness", async {
        bootstrap().await?;

        {
            let mut stream = TcpStream::connect((DAEMON, PORT)).await?;
            // Declares 4096, sends 16, then goes away. turmoil sends a FIN when
            // the receive buffer is empty and a RST when it is not; the daemon
            // has to survive either, so which one this is need not be pinned.
            stream
                .write_all(&head_with_length("POST", RUN_HOOK, None, 4096))
                .await?;
            stream.write_all(&[b'{'; 16]).await?;
            stream.flush().await?;
            drop(stream);
        }

        // Give the daemon a chance to notice and mishandle it.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut client = Client::connect().await?;
        let (status, body) = client.send(health()).await?;
        assert_eq!(
            status,
            StatusCode::OK,
            "the daemon stopped serving after a mid-body disconnect"
        );
        let health: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(
            health["bootstrapped"],
            serde_json::Value::Bool(true),
            "the daemon restarted and lost its in-memory token"
        );

        Ok(())
    });

    sim.run()?;

    // Bootstrap, the abandoned connection, and the recovery connection. A count
    // below this would mean the accept loop stopped, which is the failure this
    // scenario is really about.
    assert_eq!(
        accepted.load(Ordering::Relaxed),
        3,
        "the accept loop did not survive the abandoned connection"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 7. TLS bytes on a plaintext port
// ---------------------------------------------------------------------------

/// Raw TLS handshake bytes get a 400 and the listener stays up.
///
/// Something in the platform's path probes the port with TLS before bootstrap
/// (measured 2026-08-04; see `docs/PLATFORM.md`), so this is normal traffic
/// rather than an attack. It looks like an attack in logs, which is exactly why a
/// daemon that died on it would be diagnosed as anything but this.
#[test]
fn tls_bytes_on_the_plaintext_port_get_a_400_and_the_daemon_stays_up() -> turmoil::Result {
    let mut sim = sim(0x5EED_0008);
    spawn_daemon(&mut sim, Config::default());

    sim.client("harness", async {
        // A ClientHello record header, and the cipher-suite bytes the predecessor
        // actually logged. Both are sent because the leading bytes are what decide
        // whether hyper's version sniffing takes the HTTP/1 path at all.
        let probes: [&[u8]; 2] = [
            b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03",
            b"\x13\x01\x13\x02\x13\x03\xc0\x2b\xc0\x2f",
        ];

        for probe in probes {
            let mut stream = TcpStream::connect((DAEMON, PORT)).await?;
            stream.write_all(probe).await?;
            stream.flush().await?;

            let response = read_raw(&mut stream).await?;
            assert_eq!(
                response.status, 400,
                "a TLS probe must be answered, not dropped: {probe:?}"
            );
        }

        // And the daemon still serves real traffic afterwards, which is the part
        // that matters: the 400 is cosmetic, the survival is not.
        let mut client = Client::connect().await?;
        let (status, _) = client.send(run_hook(TOKEN)).await?;
        assert_eq!(
            status,
            StatusCode::OK,
            "the daemon stopped serving after a TLS probe"
        );

        Ok(())
    });

    sim.run()
}

// ---------------------------------------------------------------------------
// 8. The wire-level body cap
// ---------------------------------------------------------------------------

/// A body over the wire cap is 413 before it is read.
///
/// The extractor-level default does not apply to bodies consumed as a stream, so
/// the wire-level layer is the only real cap — and the predecessor measured
/// archive size *inside* the gzip `with` block, where `tell()` reported 10 bytes
/// for a 327-byte archive and the guard was nearly decorative. Declaring the
/// length and sending nothing proves the cap is decided from the header rather
/// than from bytes counted on the way past.
#[test]
fn a_body_over_the_wire_cap_is_413_without_being_read() -> turmoil::Result {
    let mut sim = sim(0x5EED_0009);
    spawn_daemon(&mut sim, config(4096, 1024));

    sim.client("harness", async {
        bootstrap().await?;

        let mut stream = TcpStream::connect((DAEMON, PORT)).await?;

        // A valid token, so the 413 is attributable to the cap rather than to
        // authorization. Nothing after the head is ever written.
        stream
            .write_all(&head_with_length(
                "PUT",
                "/v1/fs/tar?path=/tmp/never-extracted",
                Some(TOKEN),
                1_000_000,
            ))
            .await?;
        stream.flush().await?;

        let response = tokio::time::timeout(Duration::from_secs(30), read_raw(&mut stream)).await;
        let response = response.map_err(|_| {
            "no answer to a 1 MB declaration against a 4 KiB cap: the cap is being \
             enforced by counting bytes rather than by reading the header"
        })??;
        assert_eq!(
            response.status, 413,
            "a body past the wire cap was not refused from its declared length"
        );

        Ok(())
    });

    sim.run()
}

// ---------------------------------------------------------------------------
// 9. An attach is a view, not the exec
// ---------------------------------------------------------------------------

/// A stream abandoned mid-command leaves the exec running to completion.
///
/// The single most important property of the streaming design, and the reason stdin
/// is a separate POST: an exec is a server-side object and an attach is a view onto
/// it. If an attach owned any part of the exec's lifetime, an agent harness inside
/// the VM would lose a twenty-minute build to a proxy hiccup — and lose it silently,
/// seeing a closed body and a command that never reported.
///
/// The child blocks on `read` between its two `echo`s, so the drop provably lands
/// mid-command and the second `echo` is written while provably nobody is attached.
/// That byte is what a detach-kills-the-exec implementation loses, and pacing with
/// `read` rather than `sleep` is what makes "while nobody was attached" a fact the
/// test causes instead of a race it hopes for.
#[test]
fn dropping_a_stream_mid_command_does_not_disturb_the_exec() -> turmoil::Result {
    let mut sim = sim(0x5EED_0101);
    let accepted = spawn_daemon(&mut sim, patient(|_| {}));

    sim.client("harness", async {
        bootstrap().await?;
        start_exec("detach", "echo before; read -r _; echo after; exit 9", true).await?;

        {
            let mut attach = SseSocket::attach("detach", None).await?;
            let head = attach.next().await?.expect("a first output event");
            assert_eq!(head.name, "output");
            assert_eq!(String::from_utf8_lossy(&head.output()), "before\n");
            // Hang up while the child is alive and has not written its second line.
            // The socket goes with the value, so this is a transport event rather
            // than a negotiated shutdown.
            drop(attach);
        }

        // Nothing is attached. Release the child, which writes the rest into a void.
        let (status, body) = write_stdin("detach", Some(b"go\n"), true).await?;
        assert_eq!(
            status,
            StatusCode::OK,
            "the exec was unreachable after its stream was dropped: {body}"
        );

        let polled = await_exit("detach").await?;
        assert_eq!(
            polled["phase"], "exited",
            "the exec did not survive its stream being dropped: {polled}"
        );
        assert_eq!(
            polled["exit_code"], 9,
            "the exec ended on the detach's terms rather than its own: {polled}"
        );
        assert_eq!(
            polled["stdout"], "before\nafter\n",
            "output written while nothing was attached was lost: {polled}"
        );

        Ok(())
    });

    sim.run()?;

    // Bootstrap, start, the abandoned attach, the stdin write, and the poll. The
    // floor is what matters: fewer would mean the accept loop stopped after the
    // abandoned stream, which is the failure this scenario is really about.
    assert!(
        accepted.load(Ordering::Relaxed) >= 5,
        "the accept loop did not survive an abandoned stream"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 10. The offset cursor
// ---------------------------------------------------------------------------

/// A reconnect at an offset resumes exactly there: no duplicate, no hole.
///
/// This is the property E2B's cursorless `connect(pid)` cannot have, and their issue
/// #1352 is its absence. Without a cursor a reattach either replays bytes the client
/// already printed or starts wherever the server's buffer happens to begin, and
/// neither is distinguishable from correct behavior with nothing to check against.
///
/// The two attaches are concatenated and compared against the polled output, so the
/// assertion is on their union rather than on either half: a daemon that ignored the
/// query and replayed from zero would still deliver every byte, and only the seam
/// shows the difference. The child blocks on `read` at the seam so the first attach
/// cannot accidentally see past it.
#[test]
fn reconnecting_at_an_offset_resumes_exactly_there() -> turmoil::Result {
    let mut sim = sim(0x5EED_0102);
    spawn_daemon(&mut sim, patient(|_| {}));

    sim.client("harness", async {
        bootstrap().await?;
        start_exec("resume", "echo AAAA; read -r _; echo BBBB; echo CCCC", true).await?;

        let mut first = SseSocket::attach("resume", Some(0)).await?;
        let head = first.next().await?.expect("a first output event");
        assert_eq!(head.offset(), 0, "an attach at 0 must start at 0");
        let head_bytes = head.output();
        assert_eq!(String::from_utf8_lossy(&head_bytes), "AAAA\n");
        let resume_at = head_bytes.len() as u64;
        drop(first);

        // Release the child only now, so everything after the seam is written while
        // the first attach is gone and before the second exists.
        let (status, body) = write_stdin("resume", Some(b"go\n"), true).await?;
        assert_eq!(status, StatusCode::OK, "the stdin release failed: {body}");
        await_exit("resume").await?;

        let mut second = SseSocket::attach("resume", Some(resume_at)).await?;
        let rest = second.drain().await?;

        // Inside the replay window, so nothing was lost and nothing may claim to
        // have been. A gap here would be the daemon covering for a cursor it did not
        // honor.
        let tail = tile(&rest, resume_at);
        assert_eq!(
            tail.skipped, 0,
            "a reattach inside the replay window reported a gap: {rest:#?}"
        );
        assert_eq!(
            rest.iter()
                .find(|event| event.name == "output")
                .map(Sse::offset),
            Some(resume_at),
            "the first resumed event did not start at the requested offset, so the \
             offset query was ignored: {rest:#?}"
        );

        // The seam: the two attaches together are the whole output, exactly once.
        let mut joined = head_bytes.clone();
        joined.extend_from_slice(&tail.bytes);
        let polled = poll_exec("resume").await?;
        let whole = polled["stdout"].as_str().expect("stdout");
        assert_eq!(
            String::from_utf8_lossy(&joined),
            whole,
            "the two attaches did not reconstruct the output, so the resume \
             duplicated or lost bytes at the seam"
        );

        Ok(())
    });

    sim.run()
}

// ---------------------------------------------------------------------------
// 11. A starved reader
// ---------------------------------------------------------------------------

/// A reader the network starves either gets every byte or is told what it lost.
///
/// The distinction is the entire point of the `gap` event. A daemon that silently
/// drops output when a subscriber falls behind hands back a truncated log that reads
/// as a complete one, and a harness parsing it concludes a build finished quietly.
/// The honest alternative names the byte range, which a client can act on by
/// re-attaching there.
///
/// The mechanism is a one-segment TCP buffer plus a fully held link while the
/// command writes megabytes: the daemon's broadcast channel overflows for this
/// subscriber and its replay ring wraps, which is the only way to reach the `Lagged`
/// path from outside the process. [`tile`] is what makes the result checkable rather
/// than anecdotal — it walks the events as a tiling of the offset space and fails if
/// they cover less than the terminal offset names, which is exactly the shape of
/// silent loss.
#[test]
fn a_starved_reader_gets_every_byte_or_an_honest_gap() -> turmoil::Result {
    // One segment of TCP buffer, so the daemon's writes to this client back up
    // almost immediately instead of being absorbed by the simulated network.
    let mut sim = turmoil::Builder::new()
        .rng_seed(0x5EED_0103)
        .enable_tokio_io()
        .simulation_duration(Duration::from_secs(600))
        .tcp_capacity(1)
        .build();
    // A small ring and a shallow channel against a command writing megabytes, so
    // both the live channel and the replay window are certain to be outrun. The
    // keep-alive is pushed past the scenario so its comments cannot be what keeps
    // the connection's bookkeeping moving.
    spawn_daemon(
        &mut sim,
        patient(|cfg| {
            cfg.stream_buffer_bytes = 32 * 1024;
            cfg.stream_channel_capacity = 2;
            cfg.sse_keepalive = Duration::from_secs(3600);
        }),
    );

    sim.client("harness", async {
        bootstrap().await?;
        start_exec(
            "starved",
            "i=0; while [ $i -lt 60000 ]; do \
             echo LINE$i-padpadpadpadpadpadpadpadpadpadpadpadpadpadpad; i=$((i+1)); done",
            false,
        )
        .await?;

        let mut attach = SseSocket::attach("starved", Some(0)).await?;
        let head = attach.next().await?.expect("a first event");

        // Freeze delivery in both directions while the command keeps writing. This
        // is the starved reader: the socket is open, the daemon is producing, and
        // nothing is moving.
        turmoil::hold(DAEMON, "harness");
        tokio::time::sleep(Duration::from_secs(30)).await;
        turmoil::release(DAEMON, "harness");

        let rest = attach.drain().await?;
        let mut events = vec![head];
        events.extend(rest);

        // The tiling is the assertion: every byte up to the terminal offset is
        // either delivered or declared lost, contiguously, exactly once.
        let ledger = tile(&events, 0);
        assert!(
            ledger.delivered > 0,
            "nothing at all was delivered, so this scenario measured the simulated \
             transport rather than the daemon"
        );
        assert!(
            ledger.skipped > 0,
            "the reader was starved for 30 simulated seconds against a 32 KiB ring \
             and a 2-slot channel, yet nothing was reported lost: the scenario no \
             longer reaches the lagged path, so it is not testing honesty about it"
        );
        assert_eq!(
            exit_of(&events).map(Sse::offset),
            Some(ledger.delivered + ledger.skipped),
            "the terminal offset disagrees with what the events accounted for"
        );

        Ok(())
    });

    sim.run()
}

// ---------------------------------------------------------------------------
// 12. Two views on one exec
// ---------------------------------------------------------------------------

/// Two concurrent attaches each see the whole output and each terminate.
///
/// An attach that consumed from a single-consumer queue would split the output
/// between them, and each half would look complete — no gap, no error, a terminal
/// event on both. So the assertion is that each stream *independently* reconstructs
/// the polled output, not merely that both received something.
///
/// This matters beyond fan-out for its own sake: a client re-attaching because it
/// believes its stream is dead, while the original is in fact still draining, is the
/// normal recovery shape. If attaching were exclusive, that retry would corrupt the
/// view it was recovering.
#[test]
fn two_concurrent_streams_on_one_exec_both_see_everything() -> turmoil::Result {
    let mut sim = sim(0x5EED_0104);
    spawn_daemon(&mut sim, patient(|_| {}));

    sim.client("harness", async {
        bootstrap().await?;
        start_exec(
            "fanout",
            "echo one; read -r _; echo two; read -r _; echo three",
            true,
        )
        .await?;

        let mut first = SseSocket::attach("fanout", Some(0)).await?;
        let mut second = SseSocket::attach("fanout", Some(0)).await?;

        // Both attached before anything past the first line exists, so neither can
        // be replaying a finished buffer — which is how a single-consumer
        // implementation would look correct.
        let (a, b) = tokio::join!(first.next(), second.next());
        let (a_head, b_head) = (a?.expect("first sees one"), b?.expect("second sees one"));
        for head in [&a_head, &b_head] {
            assert_eq!(
                String::from_utf8_lossy(&head.output()),
                "one\n",
                "an attach did not see the first line, so the two are stealing from \
                 each other"
            );
        }

        // Drive the child the rest of the way while both are still attached.
        write_stdin("fanout", Some(b"go\n"), false).await?;
        write_stdin("fanout", Some(b"go\n"), true).await?;

        let (a_rest, b_rest) = tokio::join!(first.drain(), second.drain());
        let polled = await_exit("fanout").await?;
        let expected = polled["stdout"].as_str().expect("stdout");

        for (label, head, rest) in [("first", a_head, a_rest?), ("second", b_head, b_rest?)] {
            let mut events = vec![head];
            events.extend(rest);
            let ledger = tile(&events, 0);
            assert_eq!(
                ledger.skipped, 0,
                "the {label} attach lost bytes to the other one: {events:#?}"
            );
            assert_eq!(
                String::from_utf8_lossy(&ledger.bytes),
                expected,
                "the {label} attach saw only part of the output, so attaching is \
                 exclusive"
            );
            assert!(
                exit_of(&events).is_some(),
                "the {label} attach never terminated: {events:#?}"
            );
        }

        Ok(())
    });

    sim.run()
}

// ---------------------------------------------------------------------------
// 13. Silence versus death
// ---------------------------------------------------------------------------

/// A silent exec still sends keep-alive comments, before any output.
///
/// An agent harness thinking, or a build linking, is silent for minutes. Through a
/// proxy an idle connection is indistinguishable from a dead one, so the proxy closes
/// it and the client concludes the command died — at exactly the moment the command
/// was doing its most expensive work.
///
/// The ordering is deliberate: the comments counted here all arrived *before* the
/// first output event. A test that counted comments across the whole stream would
/// pass against a daemon that only emitted them between outputs, which is precisely
/// the case that does not need them. The child is silent because it is blocked on
/// `read`, so the silence lasts exactly as long as this test withholds the write —
/// pacing it with `sleep` would tie the scenario to the wall clock while the
/// keep-alive interval runs on the virtual one.
#[test]
fn keepalive_comments_arrive_during_silence_before_any_output() -> turmoil::Result {
    let mut sim = sim(0x5EED_0105);
    // A short interval so the scenario needs a handful of virtual intervals rather
    // than production's 15-second ones. The ratio of silence to interval is what is
    // under test, not the absolute value.
    spawn_daemon(
        &mut sim,
        patient(|cfg| cfg.sse_keepalive = Duration::from_millis(500)),
    );

    sim.client("harness", async {
        bootstrap().await?;
        start_exec("quiet", "read -r _; echo finally", true).await?;

        let mut attach = SseSocket::attach("quiet", None).await?;

        // Wait on a stream that has nothing to say. The timeout is the mechanism,
        // not a safety net: it is how the test holds the connection open through a
        // silence without the child being able to end it.
        let silent = tokio::time::timeout(Duration::from_secs(4), attach.next()).await;
        assert!(
            silent.is_err(),
            "the stream produced an event during what was supposed to be silence, \
             so this scenario is not measuring an idle connection"
        );
        let during_silence = attach.keepalives();
        assert!(
            during_silence >= 3,
            "only {during_silence} keep-alive comments arrived across 4 seconds of \
             silence at a 500 ms interval: a proxy would have closed this connection \
             and the client would read a working command as a dead one"
        );

        // And the connection that carried those comments is still a working stream.
        write_stdin("quiet", Some(b"go\n"), true).await?;
        let events = attach.drain().await?;
        assert_eq!(
            String::from_utf8_lossy(&tile(&events, 0).bytes),
            "finally\n",
            "the connection survived the silence but no longer carried output"
        );
        assert!(
            exit_of(&events).is_some(),
            "the quiet exec's stream never terminated: {events:#?}"
        );

        Ok(())
    });

    sim.run()
}

// ---------------------------------------------------------------------------
// 14. stdin end to end
// ---------------------------------------------------------------------------

/// A stdin write then EOF completes a reading child, over the network.
///
/// What this adds over the handler test is the whole path: the base64 body crosses a
/// real socket, the daemon's copy of the pipe is dropped by a request on a
/// *different* connection than the one that wrote the bytes, and the child's echo
/// comes back through a third. If the write half were tied to any of those
/// connections the child would hang — and a hang is what a caller reads as "the
/// daemon lost my prompt", the least debuggable failure available.
///
/// The intermediate poll is load-bearing: it proves the bytes arrived *without* the
/// EOF, so the completion below is attributable to the EOF rather than to the child
/// having exited for reasons of its own.
#[test]
fn a_stdin_write_then_eof_completes_a_reading_child() -> turmoil::Result {
    let mut sim = sim(0x5EED_0106);
    spawn_daemon(&mut sim, patient(|_| {}));

    sim.client("harness", async {
        bootstrap().await?;
        // `cat` blocks until EOF, so nothing here can pass by accident: a child that
        // exited on its own would not echo, and one that never sees EOF never exits.
        start_exec("feed", "cat", true).await?;

        let (status, body) = write_stdin("feed", Some(b"prompt line\n"), false).await?;
        assert_eq!(status, StatusCode::OK, "the write was refused: {body}");
        assert_eq!(
            body["written"], 12,
            "the daemon miscounted the write: {body}"
        );
        assert_eq!(body["eof"], false);

        // The bytes are in, the input is not over, so `cat` is still reading.
        let mid = poll_exec("feed").await?;
        assert_eq!(
            mid["phase"], "running",
            "cat exited without an EOF, so the write did not reach a live pipe: {mid}"
        );

        let (status, body) = write_stdin("feed", None, true).await?;
        assert_eq!(status, StatusCode::OK, "the EOF was refused: {body}");
        assert_eq!(body["eof"], true);

        let polled = await_exit("feed").await?;
        assert_eq!(
            polled["exit_code"], 0,
            "cat never saw EOF across the network: {polled}"
        );
        assert_eq!(
            polled["stdout"], "prompt line\n",
            "the bytes did not survive the round trip: {polled}"
        );

        Ok(())
    });

    sim.run()
}

// ---------------------------------------------------------------------------
// 15. A stdin POST that vanishes
// ---------------------------------------------------------------------------

/// A stdin POST abandoned mid-body leaves the exec writable on a new connection.
///
/// This is the recovery shape for a harness feeding a long-lived agent process: a
/// write times out at the client, the connection goes, and the *next* prompt has to
/// land. A daemon that closed the pipe when a write's connection died — or that left
/// the stdin mutex held by the abandoned request — would answer the retry 410 or
/// never at all, and the process on the other side would be unreachable while still
/// running.
///
/// The declared length overstates what is sent, so the daemon is genuinely waiting on
/// body bytes when the socket disappears. That is the state a handler test cannot
/// construct, because a handler is given a body that is already complete.
#[test]
fn a_stdin_post_abandoned_mid_body_leaves_stdin_writable() -> turmoil::Result {
    let mut sim = sim(0x5EED_0107);
    let accepted = spawn_daemon(&mut sim, patient(|_| {}));

    sim.client("harness", async {
        bootstrap().await?;
        start_exec("halfwrite", "cat", true).await?;

        {
            let mut stream = TcpStream::connect((DAEMON, PORT)).await?;
            // Declares 4096 and sends a truncated JSON prefix, then goes away with
            // the daemon still waiting on the rest.
            stream
                .write_all(&head_with_length(
                    "POST",
                    "/v1/exec/halfwrite/stdin",
                    Some(TOKEN),
                    4096,
                ))
                .await?;
            stream.write_all(br#"{"data_b64":"aGVsbG8"#).await?;
            stream.flush().await?;
            drop(stream);
        }

        // The exec is untouched: still running, still reading.
        let mid = poll_exec("halfwrite").await?;
        assert_eq!(
            mid["phase"], "running",
            "an abandoned stdin POST killed the exec: {mid}"
        );

        // And the write half still works, from a connection that has nothing to do
        // with the one that died.
        let (status, body) = write_stdin("halfwrite", Some(b"after the drop\n"), true).await?;
        assert_eq!(
            status,
            StatusCode::OK,
            "stdin was unusable after a write's connection died: {body}"
        );
        assert_eq!(body["written"], 15);

        let polled = await_exit("halfwrite").await?;
        assert_eq!(polled["exit_code"], 0);
        assert_eq!(
            polled["stdout"], "after the drop\n",
            "either the abandoned write's prefix leaked into the child's input or \
             the retry did not reach it: {polled}"
        );

        Ok(())
    });

    sim.run()?;

    // The daemon kept accepting after the abandoned POST, which is the half a wedged
    // connection task would take out.
    assert!(
        accepted.load(Ordering::Relaxed) >= 5,
        "the accept loop did not survive an abandoned stdin POST"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 16. Exit versus disconnect
// ---------------------------------------------------------------------------

/// The terminal event, and only it, separates exit from disconnect.
///
/// This is the reason the design is SSE rather than a raw chunked body. With bare
/// bytes, a command that finished and a connection that died are the same
/// observation — the body ends — so a client cannot tell a zero-output success from
/// a lost attach. Framing makes the difference a fact on the wire.
///
/// Both halves are asserted in one test because either alone is vacuous: a completed
/// stream carrying `exit` proves nothing if a killed one carries it too, and a killed
/// stream lacking `exit` proves nothing if a completed one lacks it as well. The
/// daemon crash is what produces a genuinely dead connection — it drops the listener
/// and every connection task at once, which is what a MicroVM losing its process
/// looks like from outside.
#[test]
fn only_the_terminal_event_separates_a_finished_exec_from_a_dead_connection() -> turmoil::Result {
    let mut sim = sim(0x5EED_0108);
    spawn_daemon(&mut sim, patient(|_| {}));

    // Set once the client has attached and read a byte, so the crash lands
    // mid-stream rather than before the attach or after the exit.
    let attached = Arc::new(AtomicUsize::new(0));
    let signal = Arc::clone(&attached);
    // Carried out of the client because a client's own assertions cannot outlive the
    // simulation, and the crash below deliberately ends this one.
    let verdict = Arc::new(std::sync::Mutex::new(None));
    let record = Arc::clone(&verdict);

    sim.client("harness", async move {
        bootstrap().await?;

        // Half one: an exec that finishes normally. Its stream carries the real exit
        // code and its body ends the way a complete response does.
        start_exec("clean", "echo done; exit 12", false).await?;
        let mut clean = SseSocket::attach("clean", None).await?;
        let events = clean.drain().await?;
        let exit = exit_of(&events).expect("a completed stream must carry a terminal event");
        assert_eq!(
            exit.data["exit_code"], 12,
            "the terminal event carried the wrong status, so a client cannot trust it \
             in place of a poll: {events:#?}"
        );
        assert!(
            clean.graceful,
            "the body did not end with its terminating chunk, so even a framed stream \
             looks truncated to a client"
        );

        // Half two: the same shape of attach against a daemon that dies under it.
        start_exec("killed", "echo alive; read -r _; echo never", true).await?;
        let mut killed = SseSocket::attach("killed", None).await?;
        let head = killed.next().await?.expect("a first output event");
        assert_eq!(String::from_utf8_lossy(&head.output()), "alive\n");
        signal.store(1, Ordering::SeqCst);

        let mut after = Vec::new();
        // Reading stops on either a FIN with no terminating chunk or a reset. Both
        // are the connection dying, and neither is a terminal event — which is the
        // fact this half of the test exists to record.
        while let Ok(Some(event)) = killed.next().await {
            after.push(event.name);
        }
        *record.lock().expect("verdict") = Some((after, killed.graceful));
        Ok(())
    });

    // Stepped by hand so the crash is timed against the client's progress. A crash
    // scheduled on virtual time alone would race the attach.
    let mut crashed = false;
    loop {
        if !crashed && attached.load(Ordering::SeqCst) == 1 {
            sim.crash(DAEMON);
            crashed = true;
        }
        if sim.step()? {
            break;
        }
    }
    assert!(
        crashed,
        "the client never attached, so nothing was under test"
    );

    let (after, graceful) = verdict
        .lock()
        .expect("verdict")
        .take()
        .expect("the client recorded what it saw after the crash");
    assert!(
        !after.iter().any(|name| name == "exit"),
        "a killed connection produced a terminal event, so the framing cannot tell a \
         finished command from a dead daemon: {after:?}"
    );
    assert!(
        !graceful,
        "the body ended with a terminating chunk even though the daemon was gone, so \
         a client would read the crash as a complete response"
    );
    Ok(())
}
