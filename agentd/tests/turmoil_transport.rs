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

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agentd::{AppState, Config, routes, serve};
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

/// The token the platform's `/run` hook installs. Long enough that a truncating
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

/// A `POST /run` carrying `token`.
fn run_hook(token: &str) -> Request<Full<Bytes>> {
    let body = format!(r#"{{"agent_token":"{token}"}}"#);
    Request::builder()
        .method("POST")
        .uri("/run")
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
                .write_all(&head_with_length("POST", "/run", None, 4096))
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
