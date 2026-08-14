// SPDX-License-Identifier: Apache-2.0
//! The in-VM client: one MicroVM's control API, with the proxy auth handled for you.
//!
//! Talks to `agentd` through the endpoint proxy — the minted proxy token read from the
//! auth-token header map and both proxy headers on every request (TRAP-7), minting
//! inside the retry path below the sixty-minute ceiling (TRAP-9), and the byte-offset
//! cursor that makes an interrupted output stream resumable.
//!
//! # A session holds no state worth keeping
//!
//! Every exec record, every file, and the bootstrap token live in the VM. So a
//! [`Session`] rebuilt from an endpoint and an agent token reattaches to everything a
//! previous process was doing, and [`ExecHandle`] rebuilt from an exec id addresses the
//! same server-side exec. That is a property of the protocol rather than of this type,
//! and it is why the exec id is caller-minted.
//!
//! # The wire types are the daemon's
//!
//! Every request body and response shape comes from the `protocol` crate, which the
//! daemon also compiles against (ARCH-2). There is deliberately no mirror here: a field
//! renamed on the daemon side breaks this build, which is the earliest and cheapest
//! place a protocol change can fail.
//!
//! # Layout
//!
//! [`proxy`] is the two-header auth and the mint schedule. [`http`] is the transport
//! seam. [`sse`] is the frame parser and the typed events. [`exec`] is the handle and
//! the cursor-driven stream. [`files`] is file and tar transfer.

pub mod exec;
pub mod files;
pub mod http;
pub mod proxy;
pub mod sse;

use std::sync::Arc;
use std::time::Duration;

pub use exec::{EndReason, ExecHandle, ExecResult, StreamEnd, StreamOptions, mint_exec_id};
pub use http::{ChunkSource, HttpBackend, HttpRequest, HttpResponse, OpenStream, ReqwestBackend};
pub use proxy::{
    Clock, DEFAULT_AGENT_PORT, DEFAULT_REFRESH_AFTER, MAX_TOKEN_LIFETIME, PROXY_AUTH_HEADER,
    PROXY_PORT_HEADER, ProxyAuth, ProxyToken, TokenMinter, TokioClock,
};
pub use sse::{ExecEvent, Frame, SseParser};

use crate::error::{Error, ErrorKind, WireKind};

/// How long one non-streaming request may take.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait for a daemon to report bootstrapped.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(120);

/// How often to re-poll health while waiting for bootstrap.
const READY_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The transport: a backend, the agent token, and the proxy auth every request needs.
///
/// Separate from [`Session`] because [`ExecHandle`] needs it and holding a whole session
/// would make the two mutually recursive.
pub struct Transport {
    backend: http::SharedBackend,
    agent_token: String,
    /// `None` means send no proxy headers at all, which is the shape for talking to a
    /// daemon directly — a local binary, a test server, or a VM reached over a tunnel.
    /// Requiring a control-plane client for that case would make this library
    /// untestable without AWS.
    proxy: Option<Arc<ProxyAuth>>,
    timeout: Duration,
}

impl Transport {
    /// The proxy auth, when this transport has one.
    pub fn proxy(&self) -> Option<&Arc<ProxyAuth>> {
        self.proxy.as_ref()
    }

    /// Every header this request needs: both proxy headers, then the bearer.
    ///
    /// `token` is three-way rather than two: `Some(Some(..))` overrides the session's
    /// token (which is how a 401 is provoked), `Some(None)` sends no `Authorization`
    /// header at all (which is how the health and hook routes are exercised), and
    /// `None` means the session's own token. A two-valued parameter could not express
    /// the middle case, and `None`-means-no-header would make the common case the
    /// dangerous one.
    async fn headers(&self, token: Option<Option<&str>>) -> Result<Vec<(String, String)>, Error> {
        // The mint sits here, inside the path every request takes, which is what makes
        // it happen at all (TRAP-9). A failure is retryable and propagates as one.
        let mut headers = match &self.proxy {
            Some(proxy) => proxy.headers().await?,
            None => Vec::new(),
        };
        let bearer = match token {
            Some(explicit) => explicit,
            None => Some(self.agent_token.as_str()),
        };
        if let Some(bearer) = bearer {
            headers.push(("Authorization".to_string(), format!("Bearer {bearer}")));
        }
        Ok(headers)
    }

    /// Sends one request and returns the response whatever its status.
    async fn request(&self, mut request: HttpRequest) -> Result<HttpResponse, Error> {
        // Prepend, never replace: the caller's headers carry the content type
        // (exec start is application/json, file upload is octet-stream), and
        // replacing the vec silently stripped them — the daemon answered 400
        // "body is not a valid start request" on the first live run while every
        // fake-backed test stayed green, because the fakes parse bodies without
        // reading content-type. The replacement was also what stripped the
        // token-intent marker, so keeping the caller's headers means dropping
        // that one explicitly — it is not a real header and must not leave.
        let mut headers = self.headers(request_token(&request)).await?;
        request.headers.retain(|(name, _)| name != TOKEN_INTENT);
        headers.append(&mut request.headers);
        request.headers = headers;
        if request.timeout.is_none() {
            request.timeout = Some(self.timeout);
        }
        self.backend.send(request).await
    }

    /// [`Self::request`], failing with the typed error for any non-2xx.
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, Error> {
        let (method, path) = (request.method, request.path.clone());
        let response = self.request(request).await?;
        response.error_for_status(method, &path)?;
        Ok(response)
    }

    /// [`Self::send`], deserializing the body.
    async fn send_json<T: serde::de::DeserializeOwned>(
        &self,
        request: HttpRequest,
    ) -> Result<T, Error> {
        let (method, path) = (request.method, request.path.clone());
        let response = self.send(request).await?;
        serde_json::from_slice(&response.body).map_err(|err| {
            // A body the client cannot read is the daemon and the client disagreeing
            // about a shape, which is what the shared protocol crate exists to prevent —
            // so reaching here means a version mismatch rather than a bad request.
            Error::wire(
                WireKind::ProtocolError,
                format!(
                    "{method} {path} answered a body this client cannot read: {err}; \
                     check /v1/schema for the daemon's protocol version"
                ),
            )
            .with_source(err)
        })
    }
}

/// A marker header naming which bearer a request wants.
///
/// Carried on the request rather than passed alongside it so [`Transport::request`] has
/// one parameter instead of two that can be transposed. Stripped before the request
/// leaves — the name is not a real header and the daemon would ignore it, but sending
/// it would leak the choice onto the wire.
const TOKEN_INTENT: &str = "x-microvms-core-token-intent";

/// The value meaning "no Authorization header at all".
const TOKEN_INTENT_NONE: &str = "none";

fn request_token(request: &HttpRequest) -> Option<Option<&str>> {
    match request.header(TOKEN_INTENT) {
        None => None,
        Some(TOKEN_INTENT_NONE) => Some(None),
        Some(explicit) => Some(Some(explicit)),
    }
}

/// Marks a request as unauthenticated.
fn unauthenticated(mut request: HttpRequest) -> HttpRequest {
    request
        .headers
        .push((TOKEN_INTENT.to_string(), TOKEN_INTENT_NONE.to_string()));
    request
}

/// The control API of one running MicroVM.
pub struct Session {
    transport: Arc<Transport>,
    endpoint: String,
    port: u16,
}

impl Session {
    /// The bearer this session authenticates with.
    ///
    /// Public because a caller who launched with a minted token needs to read it back
    /// to reattach later (the Python oracle's `Session.agent_token` property, and what
    /// the CLI's run envelope publishes for `microvm exec --agent-token`). The value is
    /// still kept out of `Debug` — readable on purpose is different from printed by
    /// accident.
    pub fn agent_token(&self) -> &str {
        &self.transport.agent_token
    }

    /// A session against `endpoint`, minting proxy tokens through `minter`.
    ///
    /// Does not talk to the VM. Constructing a session is free and re-doable, and a
    /// constructor that probed would make "do I have a session" mean "is the VM up",
    /// which are different questions with different answers during a launch.
    pub async fn connect(
        endpoint: impl Into<String>,
        agent_token: impl Into<String>,
        minter: Arc<dyn TokenMinter>,
    ) -> Result<Session, Error> {
        Self::builder(endpoint, agent_token)
            .with_minter(minter)
            .build()
    }

    /// A session with no proxy auth, for a daemon reached directly.
    ///
    /// The conformance path and every local-binary test go through here. See
    /// [`Transport::proxy`] on why this is a supported shape rather than a test-only
    /// escape hatch.
    pub fn direct(
        endpoint: impl Into<String>,
        agent_token: impl Into<String>,
    ) -> Result<Session, Error> {
        Self::builder(endpoint, agent_token).build()
    }

    /// A builder, for the cases that need a port, a timeout, or a custom backend.
    pub fn builder(endpoint: impl Into<String>, agent_token: impl Into<String>) -> SessionBuilder {
        SessionBuilder {
            endpoint: endpoint.into(),
            agent_token: agent_token.into(),
            minter: None,
            backend: None,
            port: DEFAULT_AGENT_PORT,
            timeout: DEFAULT_REQUEST_TIMEOUT,
            proxy: None,
        }
    }

    /// The endpoint this session addresses.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The port the proxy token is scoped to.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The proxy auth, for a caller that needs the mint count or an invalidation.
    pub fn proxy_auth(&self) -> Option<&Arc<ProxyAuth>> {
        self.transport.proxy()
    }

    /// Points the session at a new endpoint and drops the cached proxy token (STATE-8).
    ///
    /// The measured behaviour is that the endpoint URL does not change across
    /// suspend/resume, so this is usually a no-op on the URL — but the token drop is
    /// not: a token minted against the pre-suspend instance may no longer validate, and
    /// that rejection reads exactly like a dead daemon.
    ///
    /// Takes `&mut self` because it changes where the session points, which is not
    /// something a shared reference should be able to do mid-request. The invalidation
    /// itself is `&self` on [`ProxyAuth`], so a resume path holding only the auth can
    /// still drop the token.
    pub fn rebind(&mut self, endpoint: String) {
        self.endpoint = endpoint;
        if let Some(proxy) = self.transport.proxy() {
            proxy.invalidate();
        }
    }

    // -- health ------------------------------------------------------------

    /// Unauthenticated liveness. `bootstrapped` is the useful field.
    ///
    /// Reachable through the endpoint at all implies bootstrapped in practice — the
    /// platform forwards no external traffic until the run hook returns 200 — but a
    /// caller inside the VM, or one talking to the daemon directly, can observe the
    /// pre-bootstrap state.
    pub async fn health(&self) -> Result<protocol::health::Health, Error> {
        self.transport
            .send_json(unauthenticated(HttpRequest::new("GET", "/v1/health")))
            .await
    }

    /// Polls health until the daemon reports bootstrapped.
    ///
    /// Connection errors are expected here rather than exceptional: a VM that has just
    /// reached RUNNING commonly refuses a connection or two before the proxy path is
    /// wired up, and a mint failure is retryable by construction. A *fatal* error ends
    /// the wait immediately — retrying a 401 until the deadline is one of the two
    /// mistakes the retryable split exists to prevent.
    pub async fn wait_until_ready(
        &self,
        timeout: Duration,
    ) -> Result<protocol::health::Health, Error> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last: Option<Error> = None;
        loop {
            match self.health().await {
                Ok(health) if health.bootstrapped => return Ok(health),
                Ok(_) => {}
                Err(err) if err.retryable() => last = Some(err),
                Err(err) => return Err(err),
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                let detail = match last {
                    Some(err) => format!(" (last error: {err})"),
                    None => String::new(),
                };
                return Err(Error::new(
                    ErrorKind::Timeout,
                    format!(
                        "the daemon was not bootstrapped within {}s{detail}",
                        timeout.as_secs()
                    ),
                ));
            }
            tokio::time::sleep(READY_POLL_INTERVAL.min(deadline - now)).await;
        }
    }

    // -- exec --------------------------------------------------------------

    /// Starts a command and returns its handle. Does not wait.
    ///
    /// The `exec_id` on the request is the idempotency key. The daemon returns success
    /// for a known id without spawning a second child, so a caller whose retry must be
    /// safe across its own restart supplies a stable one.
    pub async fn run(&self, req: protocol::exec::StartRequest) -> Result<ExecHandle, Error> {
        let requested = req.exec_id.clone();
        let mut request = HttpRequest::new("POST", "/v1/exec/start");
        request
            .headers
            .push(("content-type".into(), "application/json".into()));
        request.body = serde_json::to_vec(&req).map_err(|err| {
            Error::invalid_arg(format!("the start request will not serialize: {err}"))
        })?;
        let started: protocol::exec::StartResponse = self.transport.send_json(request).await?;
        // The daemon's id wins if it answered with one. It should always be the
        // requested one — the request carries the key — but a handle built from what
        // the client *asked for* rather than what the daemon *confirmed* would address
        // nothing if that ever diverged, and every later call would 404.
        let exec_id = if started.exec_id.is_empty() {
            requested
        } else {
            started.exec_id
        };
        Ok(ExecHandle::new(Arc::clone(&self.transport), exec_id))
    }

    /// A handle for an exec started earlier, possibly by another process.
    pub fn exec(&self, exec_id: impl Into<String>) -> ExecHandle {
        ExecHandle::new(Arc::clone(&self.transport), exec_id.into())
    }

    /// Start, wait, ack. The one-shot shape, for when output is all you want.
    pub async fn run_sync(
        &self,
        req: protocol::exec::StartRequest,
        timeout: Duration,
    ) -> Result<ExecResult, Error> {
        self.run(req).await?.wait_and_ack(timeout).await
    }

    /// Signals an exec's whole process group. Returns whether anything was signalled.
    pub async fn kill(&self, exec_id: &str) -> Result<bool, Error> {
        self.exec(exec_id).kill().await
    }

    // -- file transfer -----------------------------------------------------

    /// Writes one file, creating parents. `mode` is octal as a string.
    pub async fn upload_file(
        &self,
        path: &str,
        data: &[u8],
        mode: Option<&str>,
    ) -> Result<(), Error> {
        files::upload_file(&self.transport, path, data, mode).await
    }

    /// Reads one file.
    pub async fn download_file(&self, path: &str) -> Result<Vec<u8>, Error> {
        files::download_file(&self.transport, path).await
    }

    /// Whether a path exists, distinguishing absence from every other refusal.
    pub async fn file_exists(&self, path: &str) -> Result<bool, Error> {
        files::file_exists(&self.transport, path).await
    }

    /// Extracts pre-built tar bytes under `remote`.
    pub async fn upload_tar(&self, remote: &str, archive: &[u8]) -> Result<(), Error> {
        files::upload_tar(&self.transport, remote, archive).await
    }

    /// The raw tar bytes of a remote tree.
    pub async fn download_tar(&self, remote: &str) -> Result<Vec<u8>, Error> {
        files::download_tar(&self.transport, remote).await
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The agent token is a credential and stays out.
        f.debug_struct("Session")
            .field("endpoint", &self.endpoint)
            .field("port", &self.port)
            .field("proxy", &self.transport.proxy())
            .finish()
    }
}

/// Builds a [`Session`]. See [`Session::builder`].
pub struct SessionBuilder {
    endpoint: String,
    agent_token: String,
    minter: Option<Arc<dyn TokenMinter>>,
    backend: Option<http::SharedBackend>,
    port: u16,
    timeout: Duration,
    proxy: Option<Arc<ProxyAuth>>,
}

impl SessionBuilder {
    /// Mints proxy tokens through `minter`, with the default refresh schedule.
    #[must_use]
    pub fn with_minter(mut self, minter: Arc<dyn TokenMinter>) -> Self {
        self.minter = Some(minter);
        self
    }

    /// Uses an already-built [`ProxyAuth`], for a caller that needs a custom refresh
    /// interval or clock. Wins over [`Self::with_minter`].
    #[must_use]
    pub fn with_proxy_auth(mut self, proxy: Arc<ProxyAuth>) -> Self {
        self.proxy = Some(proxy);
        self
    }

    /// Replaces the HTTP backend. The seam a simulated transport plugs into.
    #[must_use]
    pub fn with_backend(mut self, backend: http::SharedBackend) -> Self {
        self.backend = Some(backend);
        self
    }

    /// The port the proxy token is scoped to and the port header names.
    #[must_use]
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// How long one non-streaming request may take.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Builds the session.
    pub fn build(self) -> Result<Session, Error> {
        let backend = match self.backend {
            Some(backend) => backend,
            None => Arc::new(ReqwestBackend::new(&self.endpoint, self.timeout)?),
        };
        let proxy = match (self.proxy, self.minter) {
            (Some(proxy), _) => Some(proxy),
            (None, Some(minter)) => Some(Arc::new(ProxyAuth::new(minter, self.port))),
            (None, None) => None,
        };
        Ok(Session {
            transport: Arc::new(Transport {
                backend,
                agent_token: self.agent_token,
                proxy,
                timeout: self.timeout,
            }),
            endpoint: self.endpoint,
            port: self.port,
        })
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A recording backend the session tests share.
    //!
    //! It is a *recorder* rather than an assertion sink: it keeps every request head
    //! and answers from a queue the test loaded, so an assertion is written against
    //! what actually went out. A fake that asserted internally would be a fake whose
    //! expectations are invisible at the call site.

    use std::collections::HashMap;
    use std::sync::{Mutex, PoisonError};

    use futures_util::future::BoxFuture;

    use super::http::{ChunkSource, HttpBackend, HttpRequest, HttpResponse, OpenStream};
    use super::*;

    /// What a recorder should answer next.
    pub(crate) enum Reply {
        /// A status and a body.
        Body(u16, Vec<u8>),
        /// A stream: the head status, then these chunks in order.
        Chunks(u16, Vec<Vec<u8>>),
        /// A transport failure, i.e. the request never produced a status.
        Cut(&'static str),
    }

    impl Reply {
        pub(crate) fn json(status: u16, body: impl serde::Serialize) -> Self {
            Reply::Body(status, serde_json::to_vec(&body).expect("test body"))
        }

        pub(crate) fn ok(body: impl serde::Serialize) -> Self {
            Reply::json(200, body)
        }
    }

    #[derive(Default)]
    pub(crate) struct Recorder {
        seen: Mutex<Vec<HttpRequest>>,
        replies: Mutex<std::collections::VecDeque<Reply>>,
    }

    impl Recorder {
        pub(crate) fn with(replies: impl IntoIterator<Item = Reply>) -> Arc<Self> {
            Arc::new(Self {
                seen: Mutex::new(Vec::new()),
                replies: Mutex::new(replies.into_iter().collect()),
            })
        }

        /// Every request the recorder saw, in order.
        pub(crate) fn requests(&self) -> Vec<HttpRequest> {
            self.seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        pub(crate) fn last(&self) -> HttpRequest {
            self.requests()
                .pop()
                .expect("the recorder saw no request at all")
        }

        fn record(&self, request: HttpRequest) -> Reply {
            self.seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request.clone());
            self.replies
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front()
                .unwrap_or_else(|| {
                    panic!(
                        "the recorder ran out of replies at {} {}",
                        request.method, request.path
                    )
                })
        }
    }

    struct Queued(std::collections::VecDeque<Vec<u8>>);

    impl ChunkSource for Queued {
        fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, Error>> {
            Box::pin(async move { Ok(self.0.pop_front()) })
        }
    }

    impl HttpBackend for Recorder {
        fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, Error>> {
            let reply = self.record(request);
            Box::pin(async move {
                match reply {
                    Reply::Body(status, body) => Ok(HttpResponse {
                        status,
                        headers: HashMap::new(),
                        body,
                    }),
                    Reply::Chunks(status, chunks) => Ok(HttpResponse {
                        status,
                        headers: HashMap::new(),
                        body: chunks.concat(),
                    }),
                    Reply::Cut(why) => Err(Error::wire(WireKind::Transport, why)),
                }
            })
        }

        fn open_stream(
            &self,
            request: HttpRequest,
            _idle_timeout: Duration,
        ) -> BoxFuture<'_, Result<OpenStream, Error>> {
            let reply = self.record(request);
            Box::pin(async move {
                let (status, chunks) = match reply {
                    Reply::Chunks(status, chunks) => (status, chunks),
                    Reply::Body(status, body) => (status, vec![body]),
                    Reply::Cut(why) => return Err(Error::wire(WireKind::Transport, why)),
                };
                let head = HttpResponse {
                    status,
                    headers: HashMap::new(),
                    // A failing head carries its body, as the real backend does, so the
                    // typed error keeps the daemon's detail string.
                    body: if (200..300).contains(&status) {
                        Vec::new()
                    } else {
                        chunks.concat()
                    },
                };
                let source: Box<dyn ChunkSource> = if (200..300).contains(&status) {
                    Box::new(Queued(chunks.into_iter().collect()))
                } else {
                    Box::new(Queued(std::collections::VecDeque::new()))
                };
                Ok((head, source))
            })
        }
    }

    /// A session over a recorder, with proxy auth wired to a counting minter.
    pub(crate) fn session_with(
        recorder: Arc<Recorder>,
    ) -> (Session, Arc<ProxyAuth>, Arc<proxy::testing::ManualClock>) {
        let clock = Arc::new(proxy::testing::ManualClock::default());
        let auth = Arc::new(
            ProxyAuth::with_refresh_after(
                Arc::new(proxy::testing::CountingMinter::default()),
                DEFAULT_AGENT_PORT,
                DEFAULT_REFRESH_AFTER,
                Arc::clone(&clock) as Arc<dyn Clock>,
            )
            .expect("the default interval is below the ceiling"),
        );
        let session = Session::builder("https://vm.example", "agent-token-abcdef")
            .with_backend(recorder)
            .with_proxy_auth(Arc::clone(&auth))
            .build()
            .expect("builds");
        (session, auth, clock)
    }

    pub(crate) fn health_body(bootstrapped: bool) -> serde_json::Value {
        serde_json::json!({
            "version": "0.1.0",
            "bootstrapped": bootstrapped,
            "disk": null,
            "identity_degraded": false,
            "identity_repaired": true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{Recorder, Reply, health_body, session_with};
    use super::*;
    use crate::session::proxy::testing::CountingMinter;

    fn start_request(exec_id: &str) -> protocol::exec::StartRequest {
        protocol::exec::StartRequest {
            exec_id: exec_id.to_string(),
            command: vec!["/bin/true".to_string()],
            shell: false,
            cwd: None,
            env: Default::default(),
            user: None,
            group: None,
            timeout_sec: None,
            stdin: false,
        }
    }

    fn header(request: &HttpRequest, name: &str) -> Option<String> {
        request.header(name).map(str::to_string)
    }

    /// The live-run regression: `Transport::request` REPLACED the header vec with
    /// the auth headers, stripping the content type the caller set — the daemon
    /// answered 400 "body is not a valid start request" against real axum while
    /// every fake stayed green (fakes parse bodies without reading content-type).
    ///
    /// **Guard proof.** Change the prepend back to `request.headers = auth_headers`
    /// and this test is red on the content-type assertion.
    #[tokio::test]
    async fn a_callers_content_type_survives_the_auth_header_injection() {
        let recorder = Recorder::with([Reply::ok(
            serde_json::json!({"exec_id":"e1","phase":"running"}),
        )]);
        let (session, _, _) = session_with(Arc::clone(&recorder));

        session.run(start_request("e1")).await.expect("start");

        let seen = recorder.requests();
        assert_eq!(
            header(&seen[0], "content-type").as_deref(),
            Some("application/json"),
            "the exec start lost its content type to the auth header injection"
        );
    }

    /// The read-back the CLI's run envelope depends on: a caller who launched with a
    /// minted token can only reattach if the session hands the token back. The first
    /// live run published `agentToken: null` because nothing read it — the daemon then
    /// saw a bootstrap replay from the literal string "None".
    #[tokio::test]
    async fn the_session_hands_back_the_token_it_authenticates_with() {
        let recorder = Recorder::with([]);
        let (session, _, _) = session_with(Arc::clone(&recorder));
        assert_eq!(session.agent_token(), "agent-token-abcdef");
    }

    /// TRAP-7's send side, asserted on a recorded request rather than on a return
    /// value: both proxy headers went out.
    ///
    /// **Guard proof.** Delete the `headers.push((PROXY_PORT_HEADER, ..))` branch in
    /// `ProxyAuth::headers_from` and this test is red on the port assertion while every
    /// other test in the crate stays green.
    #[tokio::test]
    async fn every_endpoint_request_carries_both_proxy_headers() {
        let recorder = Recorder::with([
            Reply::ok(health_body(true)),
            Reply::ok(serde_json::json!({"exec_id":"e1","phase":"running"})),
        ]);
        let (session, _, _) = session_with(Arc::clone(&recorder));

        session.health().await.expect("health succeeds");
        session.run(start_request("e1")).await.expect("start");

        let seen = recorder.requests();
        assert_eq!(seen.len(), 2);
        for request in &seen {
            assert_eq!(
                header(request, PROXY_AUTH_HEADER).as_deref(),
                Some(CountingMinter::value(0).as_str()),
                "{} {} went out without the proxy auth header",
                request.method,
                request.path
            );
            assert_eq!(
                header(request, PROXY_PORT_HEADER).as_deref(),
                Some("9000"),
                "{} {} went out without the proxy port header, which the proxy \
                 rejects as if the token were bad",
                request.method,
                request.path
            );
        }
    }

    /// Health is unauthenticated and everything else is not.
    ///
    /// Both halves matter: health carrying a bearer would still work, but it would make
    /// the route untestable as the unauthenticated route it is, and an exec route
    /// *without* one would 401.
    #[tokio::test]
    async fn health_sends_no_bearer_and_an_exec_route_does() {
        let recorder = Recorder::with([
            Reply::ok(health_body(true)),
            Reply::ok(serde_json::json!({"exec_id":"e1","phase":"running"})),
        ]);
        let (session, _, _) = session_with(Arc::clone(&recorder));

        session.health().await.expect("health");
        let seen = recorder.requests();
        assert_eq!(header(&seen[0], "authorization"), None);
        assert_eq!(
            header(&seen[0], "x-microvms-core-token-intent"),
            None,
            "the internal token-intent marker must not reach the wire"
        );

        session.run(start_request("e1")).await.expect("start");
        assert_eq!(
            header(&recorder.last(), "authorization").as_deref(),
            Some("Bearer agent-token-abcdef")
        );
    }

    /// A session with no minter sends no proxy headers at all, which is the shape for
    /// a daemon reached directly.
    #[tokio::test]
    async fn a_direct_session_sends_no_proxy_headers() {
        let recorder = Recorder::with([Reply::ok(health_body(true))]);
        let session = Session::builder("http://127.0.0.1:9000", "token")
            .with_backend(Arc::clone(&recorder) as http::SharedBackend)
            .build()
            .expect("builds");
        assert!(session.proxy_auth().is_none());

        session.health().await.expect("health");
        let request = recorder.last();
        assert_eq!(header(&request, PROXY_AUTH_HEADER), None);
        assert_eq!(header(&request, PROXY_PORT_HEADER), None);
    }

    /// A mint failure surfaces as a retryable error from the request, not from
    /// construction — the whole point of minting in the request path.
    #[tokio::test]
    async fn a_mint_failure_surfaces_as_a_retryable_request_error() {
        let recorder = Recorder::with([Reply::ok(health_body(true))]);
        let auth = Arc::new(
            ProxyAuth::with_refresh_after(
                Arc::new(CountingMinter::failing(1)),
                DEFAULT_AGENT_PORT,
                DEFAULT_REFRESH_AFTER,
                Arc::new(proxy::testing::ManualClock::default()),
            )
            .expect("interval accepted"),
        );
        let session = Session::builder("https://vm.example", "token")
            .with_backend(Arc::clone(&recorder) as http::SharedBackend)
            .with_proxy_auth(Arc::clone(&auth))
            .build()
            .expect("builds");

        let err = session.health().await.expect_err("the mint fails");
        assert_eq!(err.kind(), ErrorKind::Retryable);
        assert_eq!(err.wire_kind(), Some(WireKind::AuthTokenMint));
        assert!(
            recorder.requests().is_empty(),
            "a request went out without a token"
        );

        session.health().await.expect("the retry succeeds");
        assert_eq!(auth.mint_count(), 1);
    }

    /// Every status the daemon chooses arrives as its own wire kind, from the session's
    /// own surface rather than from the transport in isolation.
    #[tokio::test]
    async fn a_daemon_status_arrives_as_its_typed_error() {
        for (status, expected) in [
            (401, WireKind::Unauthorized),
            (404, WireKind::NotFound),
            (409, WireKind::Conflict),
            (503, WireKind::NotBootstrapped),
        ] {
            let recorder = Recorder::with([Reply::Body(status, b"{\"error\":\"x\"}".to_vec())]);
            let (session, _, _) = session_with(recorder);
            let err = session.health().await.expect_err("a failing status");
            assert_eq!(err.wire_kind(), Some(expected), "status {status}");
        }
    }

    /// `wait_until_ready` returns as soon as the daemon reports bootstrapped, and
    /// tolerates the pre-bootstrap answers before it.
    #[tokio::test(start_paused = true)]
    async fn wait_until_ready_polls_through_not_bootstrapped_and_a_dropped_connection() {
        let recorder = Recorder::with([
            Reply::Cut("connection refused"),
            Reply::Body(503, b"not bootstrapped".to_vec()),
            Reply::ok(health_body(false)),
            Reply::ok(health_body(true)),
        ]);
        let (session, _, _) = session_with(Arc::clone(&recorder));

        let health = session
            .wait_until_ready(DEFAULT_READY_TIMEOUT)
            .await
            .expect("the daemon comes up");
        assert!(health.bootstrapped);
        assert_eq!(recorder.requests().len(), 4);
    }

    /// A fatal error ends the wait immediately: retrying a 401 until the deadline is
    /// the mistake the retryable split exists to prevent.
    #[tokio::test(start_paused = true)]
    async fn wait_until_ready_gives_up_at_once_on_a_credential_failure() {
        let recorder = Recorder::with([Reply::Body(401, b"wrong token".to_vec())]);
        let (session, _, _) = session_with(Arc::clone(&recorder));

        let err = session
            .wait_until_ready(DEFAULT_READY_TIMEOUT)
            .await
            .expect_err("a 401 is fatal");
        assert_eq!(err.kind(), ErrorKind::Credentials);
        assert_eq!(
            recorder.requests().len(),
            1,
            "a fatal error was retried, so a wrong token costs the full timeout"
        );
    }

    /// The timeout is a client-side deadline and names the last retryable error, so a
    /// caller can tell "never answered" from "answered, not ready".
    #[tokio::test(start_paused = true)]
    async fn wait_until_ready_times_out_naming_the_last_retryable_error() {
        let recorder = Recorder::with([
            Reply::Cut("connection refused"),
            Reply::Cut("connection refused"),
            Reply::Cut("connection refused"),
        ]);
        let (session, _, _) = session_with(recorder);

        let err = session
            .wait_until_ready(Duration::from_secs(4))
            .await
            .expect_err("the daemon never comes up");
        assert_eq!(err.kind(), ErrorKind::Timeout);
        assert!(
            err.to_string().contains("connection refused"),
            "the timeout must name what it kept hitting: {err}"
        );
    }

    /// `rebind` drops the cached token, so the next request mints (STATE-8).
    ///
    /// The mint count is the assertion because a resumed VM keeps its endpoint URL:
    /// nothing about the *request* changes, and the only observable is that a fresh
    /// token was fetched.
    #[tokio::test]
    async fn rebind_invalidates_the_proxy_token_so_the_next_request_mints() {
        let recorder = Recorder::with([Reply::ok(health_body(true)), Reply::ok(health_body(true))]);
        let (mut session, auth, _) = session_with(Arc::clone(&recorder));

        session.health().await.expect("health");
        assert_eq!(auth.mint_count(), 1);

        session.rebind("https://vm.example".to_string());
        assert!(!auth.is_cached(), "rebind did not drop the token");

        session.health().await.expect("health");
        assert_eq!(auth.mint_count(), 2);
        assert_eq!(
            header(&recorder.last(), PROXY_AUTH_HEADER).as_deref(),
            Some(CountingMinter::value(1).as_str()),
            "the request after a rebind carried the pre-suspend token"
        );
    }

    /// A start request's body is the protocol crate's serialization, and the handle
    /// addresses the id the daemon confirmed.
    #[tokio::test]
    async fn a_start_request_serializes_through_the_protocol_crate() {
        let recorder = Recorder::with([Reply::ok(
            serde_json::json!({"exec_id":"e-confirmed","phase":"running"}),
        )]);
        let (session, _, _) = session_with(Arc::clone(&recorder));

        let handle = session.run(start_request("e-asked")).await.expect("start");
        assert_eq!(
            handle.exec_id(),
            "e-confirmed",
            "the handle addresses the id the client asked for rather than the one the \
             daemon confirmed, so every later call would 404 if they diverged"
        );

        let request = recorder.last();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/exec/start");
        let body: serde_json::Value = serde_json::from_slice(&request.body).expect("json body");
        assert_eq!(body["exec_id"], "e-asked");
        assert_eq!(body["command"], serde_json::json!(["/bin/true"]));
    }

    /// A body this client cannot read is reported as a protocol failure naming the
    /// schema route, not as a transport error.
    #[tokio::test]
    async fn an_unreadable_response_body_names_the_schema_route() {
        let recorder = Recorder::with([Reply::Body(200, b"{\"unexpected\":true}".to_vec())]);
        let (session, _, _) = session_with(recorder);
        let err = session.health().await.expect_err("the body will not parse");
        assert_eq!(err.wire_kind(), Some(WireKind::ProtocolError));
        assert!(err.to_string().contains("/v1/schema"), "{err}");
    }

    /// A session's `Debug` does not print the agent token.
    #[test]
    fn a_session_debug_does_not_print_the_agent_token() {
        let recorder = Recorder::with([]);
        let session = Session::builder("https://vm.example", "super-secret-token")
            .with_backend(recorder as http::SharedBackend)
            .build()
            .expect("builds");
        let rendered = format!("{session:?}");
        assert!(rendered.contains("vm.example"), "{rendered}");
        assert!(
            !rendered.contains("super-secret"),
            "the agent token reached a Debug string: {rendered}"
        );
    }
}
