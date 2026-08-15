// SPDX-License-Identifier: Apache-2.0
//! The endpoint proxy's authentication: two headers, and a token minted inside the
//! request path.
//!
//! # Two headers, not one (TRAP-7)
//!
//! `X-aws-proxy-auth` carries a JWE scoped to one MicroVM and a set of ports.
//! `X-aws-proxy-port` names which of that token's allowed ports *this* request
//! targets. Omitting the second is a rejection that reads like a bad token, which is
//! the worst available diagnostic: the header that is wrong is not the header the
//! error mentions. Both measured 2026-08-05; see `docs/PLATFORM.md`, "Endpoint
//! authentication".
//!
//! The other half of the same finding is the shape of the token. `CreateMicrovmAuthToken`
//! answers with `authToken` as a **map of header name to value**, not a bare string,
//! because the API is shaped for schemes needing more than one header. Reading that
//! map as a string is the trap, and [`ProxyToken`] closes it by construction: it holds
//! a map, exposes no `as_str`, has no `Display`, and the only way to reach the auth
//! value is [`ProxyToken::auth_value`], which names the header it reads.
//!
//! # Minting inside the request path (TRAP-9)
//!
//! The service caps a proxy token at sixty minutes. That is not a choice, and it is
//! shorter than a long agent run — so a token minted once at construction expires
//! mid-trial, and the resulting rejection is indistinguishable from a daemon that
//! died. Minting therefore happens in [`ProxyAuth::headers`], which every request
//! calls, and a stale cache re-mints there.
//!
//! [`DEFAULT_REFRESH_AFTER`] is half the ceiling rather than just under it. Refreshing
//! at fifty-nine minutes puts the expiry inside the window between building the
//! headers and the proxy validating them: the token is live when the request is
//! written and dead when it is read. Half the ceiling means a request in flight across
//! the rollover is still holding a token with about thirty minutes left.
//!
//! A mint failure is [`WireKind::AuthTokenMint`], which is retryable, and that is
//! load-bearing rather than optimistic: a control-plane throttle at minute thirty must
//! not kill a trial that is otherwise healthy.
//!
//! # A WebSocket carries the same two facts as subprotocols, not as headers
//!
//! The browser `WebSocket` constructor cannot set a request header, so the platform moves
//! both facts into the one field a handshake *does* let a client choose:
//! `Sec-WebSocket-Protocol`. Three values, in this order — [`WS_SUBPROTOCOL`], then the
//! token under [`WS_AUTH_SUBPROTOCOL_PREFIX`], then the target port under
//! [`WS_PORT_SUBPROTOCOL_PREFIX`] — and "Lambda removes MicroVM-specific subprotocols from
//! the request before forwarding it to your application", so an in-VM server never sees
//! them and must not be written to expect them.
//!
//! [`ProxyAuth::subprotocols`] mints through the same cache and the same refresh schedule
//! as [`ProxyAuth::headers`]. That is the whole point of it living here rather than in a
//! caller: a second token path would be a second place TRAP-9 has to be got right, and the
//! one that is not on the hot path is the one that is wrong.
//!
//! # Nothing here logs the token, and that is by construction rather than by care
//!
//! [`ProxyToken`]'s `Debug` names keys only, and [`ProxyAuth`]'s prints a mint count and a
//! cached flag. The values that *do* carry the credential — the header pairs and the
//! subprotocol strings — are returned to a caller and stored on nothing, so there is no
//! type whose `Debug` could leak one. A caller that logs what it was handed is outside
//! what this module can prevent, which is why both accessors say so in their own docs.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use futures_util::future::BoxFuture;

use crate::error::{Error, WireKind};

/// The header carrying the minted JWE. One of the two keys read out of the
/// `authToken` map.
pub const PROXY_AUTH_HEADER: &str = "X-aws-proxy-auth";

/// The header naming which of the token's allowed ports this request targets.
///
/// Sent on every request, never inferred. See the module docs for what its absence
/// looks like.
pub const PROXY_PORT_HEADER: &str = "X-aws-proxy-port";

/// The port the daemon listens on in the images this repo builds (`AGENTD_PORT`).
pub const DEFAULT_AGENT_PORT: u16 = 9000;

/// The base subprotocol every MicroVM WebSocket handshake offers first.
///
/// Bare, with no suffix: it is the marker that says the other two values in the list are
/// the platform's rather than the application's.
pub const WS_SUBPROTOCOL: &str = "lambda-microvms";

/// The prefix the JWE rides behind in a WebSocket handshake.
///
/// The subprotocol counterpart of [`PROXY_AUTH_HEADER`]. A handshake carries the token
/// here because `Sec-WebSocket-Protocol` is the only field the browser `WebSocket`
/// constructor lets a client set.
pub const WS_AUTH_SUBPROTOCOL_PREFIX: &str = "lambda-microvms.authentication.";

/// The prefix the target port rides behind in a WebSocket handshake.
///
/// The subprotocol counterpart of [`PROXY_PORT_HEADER`], and it is present for the same
/// reason: the token is scoped to a *set* of ports and the request has to name which one.
/// Omitting it is the same rejection-that-reads-like-a-bad-token described in the module
/// docs.
pub const WS_PORT_SUBPROTOCOL_PREFIX: &str = "lambda-microvms.port.";

/// The ceiling the service enforces on a proxy token's life. Not a choice.
pub const MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(60 * 60);

/// When to re-mint: half [`MAX_TOKEN_LIFETIME`]. See the module docs for why not
/// fifty-nine minutes.
pub const DEFAULT_REFRESH_AFTER: Duration = Duration::from_secs(30 * 60);

/// The minted token, as the control plane returns it: a map of header name to value.
///
/// Deliberately opaque. There is no `as_str`, no `Display`, and no `Deref`, because
/// the TRAP-7 mistake is treating this value as the token string — and a type that
/// cannot be printed as one cannot be used as one. [`ProxyToken::auth_value`] is the
/// only way out, and it names the header it reads so the call site says which key of
/// the map it means.
#[derive(Clone, Default)]
pub struct ProxyToken {
    headers: HashMap<String, String>,
}

impl ProxyToken {
    /// Wraps the `authToken` map from a `CreateMicrovmAuthToken` response.
    pub fn new(headers: HashMap<String, String>) -> Self {
        Self { headers }
    }

    /// The map as pairs, for a caller that already has them.
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self::new(
            pairs
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect(),
        )
    }

    /// One header's value, matched case-insensitively.
    ///
    /// Case-insensitively because HTTP header names are, and the map's keys come from
    /// a service response rather than from this crate: a client that matched
    /// `X-aws-proxy-auth` exactly would break on a response that spelled it
    /// `x-aws-proxy-auth`, and the failure would be a missing header rather than a
    /// missing key.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Every header the token carries, in unspecified order.
    ///
    /// Public because the platform's stated reason for a map is that a scheme may
    /// need more than one header, so a client that forwarded only the two it knows
    /// about would silently drop a third the service started sending.
    pub fn headers(&self) -> impl Iterator<Item = (&str, &str)> {
        self.headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// The [`PROXY_AUTH_HEADER`] value, or a retryable mint failure if the map has no
    /// such key.
    ///
    /// Retryable rather than a protocol error: an `authToken` map without the auth
    /// header is a control-plane response this client cannot use, which is the same
    /// situation as a mint that failed outright, and the same remedy applies.
    pub fn auth_value(&self) -> Result<&str, Error> {
        self.get(PROXY_AUTH_HEADER).ok_or_else(|| {
            Error::wire(
                WireKind::AuthTokenMint,
                format!(
                    "the minted authToken map has no {PROXY_AUTH_HEADER} key (it carries {:?}); \
                     the response is a header map, not a token string — see docs/PLATFORM.md, \
                     \"Endpoint authentication\"",
                    self.headers.keys().collect::<Vec<_>>()
                ),
            )
        })
    }

    /// Whether the map carries anything at all.
    pub fn is_empty(&self) -> bool {
        self.headers.is_empty()
    }
}

/// The seam to the control plane's own token type.
///
/// The two lanes that landed this module and [`crate::control`] arrived at the same trap
/// closure from different directions: `control::ProxyToken` keeps the auth value and the
/// port as two fields and emits a fixed pair from `headers()`, while this one keeps the
/// service's whole map so a third header the platform adds later rides along without a
/// code change. Neither is wrong, and unifying them would mean one lane editing the
/// other's module.
///
/// So the conversion lives here, which is the side that consumes it. It is the whole of
/// what a caller needs to hand `ControlPlane::mint_auth_token`'s result to a
/// [`TokenMinter`], and it is `From` rather than a named function so the wiring is one
/// `.into()` at the boundary.
impl From<crate::control::ProxyToken> for ProxyToken {
    fn from(token: crate::control::ProxyToken) -> Self {
        Self::from_pairs(token.headers())
    }
}

/// Names the keys and never the values.
///
/// A proxy token is a credential, and a `Debug` that printed it would put it in every
/// log line that formats a [`ProxyAuth`] or an error chain containing one.
impl fmt::Debug for ProxyToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut keys: Vec<&str> = self.headers.keys().map(String::as_str).collect();
        keys.sort_unstable();
        f.debug_struct("ProxyToken")
            .field("headers", &keys)
            .finish()
    }
}

/// Mints a proxy token. One method, so a test needs no AWS.
///
/// Narrow on purpose: the control plane's `mint_auth_token` implements this and
/// nothing else about the control plane is reachable from a session. A `Session`
/// coupled to the whole control-plane client would be a session that cannot be built
/// without credentials, and the conformance path talks to a local daemon.
///
/// Boxed future rather than `async fn` in the trait: a `Session` holds
/// `Arc<dyn TokenMinter>`, and an `async fn` in a trait is not dyn-compatible.
pub trait TokenMinter: Send + Sync {
    fn mint(&self) -> BoxFuture<'_, Result<ProxyToken, Error>>;
}

/// A monotonic clock, injectable so a refresh boundary is a test rather than a wait.
///
/// The production implementation is [`TokioClock`], and it is tokio's clock rather
/// than `std`'s for a reason that is easy to get backwards: under a deterministic
/// simulator the tokio clock is virtual and `std::time::Instant` is not, so a
/// sixty-minute boundary crossed in simulated time is invisible to `std`. A client
/// that timed its token against `std::time::Instant` would pass a seventy-minute
/// simulation without ever re-minting.
pub trait Clock: Send + Sync + fmt::Debug {
    /// Time since this clock was created.
    fn elapsed(&self) -> Duration;
}

/// The production clock. Virtual under turmoil, real otherwise.
#[derive(Debug)]
pub struct TokioClock {
    base: tokio::time::Instant,
}

impl Default for TokioClock {
    fn default() -> Self {
        Self {
            base: tokio::time::Instant::now(),
        }
    }
}

impl Clock for TokioClock {
    fn elapsed(&self) -> Duration {
        self.base.elapsed()
    }
}

/// One cached token and when it was minted.
struct Cached {
    token: ProxyToken,
    minted_at: Duration,
}

/// Mints, caches, and refreshes the proxy token for one MicroVM and one port.
///
/// Shared behind an `Arc` and mutated through `&self`, because every request needs
/// the headers and a request path that needed `&mut` would serialize the whole
/// session behind one lock.
pub struct ProxyAuth {
    minter: Arc<dyn TokenMinter>,
    port: u16,
    refresh_after: Duration,
    clock: Arc<dyn Clock>,
    /// The cached token. A `std::sync::RwLock` rather than tokio's, because it is only
    /// ever held for a read or a write of one field and never across an await — the
    /// await is under `mint_lock`.
    cached: RwLock<Option<Cached>>,
    /// Held across the mint so two concurrent requests arriving on an expired token
    /// produce one mint rather than two. tokio's, because minting is async.
    mint_lock: tokio::sync::Mutex<()>,
    mint_count: AtomicU64,
}

impl ProxyAuth {
    /// A [`ProxyAuth`] with the validated defaults.
    pub fn new(minter: Arc<dyn TokenMinter>, port: u16) -> Self {
        // Unwrap-free: `DEFAULT_REFRESH_AFTER` is a constant this crate owns and the
        // test below asserts it satisfies the guard, so the fallible constructor
        // cannot fail here.
        Self {
            minter,
            port,
            refresh_after: DEFAULT_REFRESH_AFTER,
            clock: Arc::new(TokioClock::default()),
            cached: RwLock::new(None),
            mint_lock: tokio::sync::Mutex::new(()),
            mint_count: AtomicU64::new(0),
        }
    }

    /// A [`ProxyAuth`] with an explicit refresh interval and clock.
    ///
    /// Refuses an interval at or above [`MAX_TOKEN_LIFETIME`] (S2, TRAP-9). An
    /// interval *at* the ceiling is the specific mistake: it looks like "refresh when
    /// the token expires", and it means every refresh races the expiry it exists to
    /// avoid. Refused locally rather than documented, because the symptom is a 401
    /// from the proxy that reads as a wrong agent token.
    pub fn with_refresh_after(
        minter: Arc<dyn TokenMinter>,
        port: u16,
        refresh_after: Duration,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, Error> {
        if refresh_after >= MAX_TOKEN_LIFETIME {
            return Err(Error::invalid_arg(format!(
                "a proxy-token refresh interval of {}s is not below the service's \
                 {}s ceiling, so a request can be issued with a token that has already \
                 expired; see docs/PLATFORM.md, \"Endpoint authentication\"",
                refresh_after.as_secs(),
                MAX_TOKEN_LIFETIME.as_secs()
            )));
        }
        Ok(Self {
            minter,
            port,
            refresh_after,
            clock,
            cached: RwLock::new(None),
            mint_lock: tokio::sync::Mutex::new(()),
            mint_count: AtomicU64::new(0),
        })
    }

    /// The port this token is scoped to, and the [`PROXY_PORT_HEADER`] value.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// How many tokens have been minted over this instance's life.
    ///
    /// Public because it is the only externally visible evidence that minting happens
    /// in the request path at all: a token cached forever and a token refreshed on
    /// schedule produce identical successful requests, and the count is what
    /// distinguishes them. STATE-8's resume test asserts on it.
    pub fn mint_count(&self) -> u64 {
        self.mint_count.load(Ordering::SeqCst)
    }

    /// Drops the cached token so the next request mints a fresh one (STATE-8).
    ///
    /// Called after a resume. The measured behaviour is that a resumed VM keeps its
    /// endpoint URL and its bootstrap state, but a token minted against the
    /// pre-suspend instance is not guaranteed to survive — and a stale-token rejection
    /// there reads exactly like a daemon that died.
    ///
    /// Synchronous, so a state machine can call it from a non-async transition.
    pub fn invalidate(&self) {
        *self.cached.write().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Whether a token is cached and still inside the refresh window.
    ///
    /// Exposed for the resume lane: after [`Self::invalidate`] this is false, which is
    /// a fact a test can assert without issuing a request.
    pub fn is_cached(&self) -> bool {
        self.fresh_token().is_some()
    }

    /// Every header this request needs, minting first if the cache is stale.
    ///
    /// Both headers, always. The auth value is read out of the token's map by name;
    /// the rest of the map rides along in case the platform adds a third header; and
    /// the port header is filled from this instance's port unless the token already
    /// named one.
    pub async fn headers(&self) -> Result<Vec<(String, String)>, Error> {
        Ok(self.headers_from(&self.token().await?))
    }

    /// Both required headers, for a port this instance was **not** built with.
    ///
    /// What a caller opening its own connection to some other port on the same VM needs —
    /// the harness contract's `getPortEndpoint() -> {url, headers}` is exactly this shape,
    /// and reimplementing it outside this module would mean a second mint path (see the
    /// module docs on why there is one).
    ///
    /// The requested port **wins over one the token map carries**, which is the opposite of
    /// [`Self::headers`]. The reason is that a caller naming a port is answering the
    /// question the header asks, and quietly substituting a different port would send a
    /// request the caller can neither predict nor debug: the rejection names the token.
    /// [`Self::headers`] defers to the map instead because nobody named a port there.
    ///
    /// The auth value is a bearer credential. Nothing in this crate logs the returned pairs
    /// and no type here stores them; a caller that logs them is past what this module can
    /// prevent.
    pub async fn headers_for_port(&self, port: u16) -> Result<Vec<(String, String)>, Error> {
        let token = self.token().await?;
        let mut headers: Vec<(String, String)> = token
            .headers()
            .filter(|(name, _)| !name.eq_ignore_ascii_case(PROXY_PORT_HEADER))
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        headers.push((PROXY_PORT_HEADER.to_string(), port.to_string()));
        Ok(headers)
    }

    /// The three subprotocols a WebSocket handshake through the endpoint proxy offers.
    ///
    /// In wire order: [`WS_SUBPROTOCOL`], the JWE behind
    /// [`WS_AUTH_SUBPROTOCOL_PREFIX`], and `port` behind [`WS_PORT_SUBPROTOCOL_PREFIX`].
    /// See the module docs for why a handshake carries these instead of the two headers,
    /// and note that the platform strips all three before forwarding — so a server inside
    /// the VM never negotiates them.
    ///
    /// Minted through the same cache and refresh schedule as [`Self::headers`]: a
    /// long-lived connection opened at minute fifty gets a token minted at minute fifty,
    /// not the one from construction, and nothing here can mint a token this instance's
    /// schedule does not know about.
    ///
    /// The middle string **contains the credential**. Same rule as
    /// [`Self::headers_for_port`]: this crate stores and logs none of it.
    pub async fn subprotocols(&self, port: u16) -> Result<[String; 3], Error> {
        let token = self.token().await?;
        // Through `auth_value` rather than by reading the map with a default, so a token
        // whose map lacks the auth key is a retryable mint failure and never a handshake
        // offering `lambda-microvms.authentication.` with nothing after it.
        //
        // Belt and braces with the identical check in `token()`'s mint path, which is where
        // the guard test actually fires today. Kept anyway: `token()` only checks a token it
        // *minted*, so an unusable map that arrived some other way — a cache seeded by a
        // future constructor, or that check being moved — would reach here, and the failure
        // it would cause is a handshake rejected for a reason naming neither the token nor
        // the port.
        let auth = token.auth_value()?;
        Ok([
            WS_SUBPROTOCOL.to_string(),
            format!("{WS_AUTH_SUBPROTOCOL_PREFIX}{auth}"),
            format!("{WS_PORT_SUBPROTOCOL_PREFIX}{port}"),
        ])
    }

    /// The cached token, minting first if the cache is stale.
    ///
    /// The one mint path. [`Self::headers`], [`Self::headers_for_port`], and
    /// [`Self::subprotocols`] all reach the control plane through here, which is what makes
    /// "minting happens in the request path" (TRAP-9) a property of the type rather than of
    /// each accessor remembering to do it.
    async fn token(&self) -> Result<ProxyToken, Error> {
        if let Some(token) = self.fresh_token() {
            return Ok(token);
        }
        // Serialize the mint. Re-check under the lock rather than minting
        // unconditionally: while this task waited, another may have refreshed, and a
        // second mint would burn a control-plane call for nothing.
        let _guard = self.mint_lock.lock().await;
        if let Some(token) = self.fresh_token() {
            return Ok(token);
        }

        let token = self.minter.mint().await.map_err(|err| {
            // Reclassified rather than passed through: whatever the minter's own
            // failure was, from a caller's point of view this is a mint failure and
            // it is retryable. A minter that reported, say, a throttle as
            // `ServerError` would still be retryable, but one that reported it as a
            // protocol error would abort a healthy trial.
            let message = format!(
                "could not mint a proxy auth token for port {}: {err}; minting is inside \
                 the request path, so the identical request may succeed",
                self.port
            );
            Error::wire(WireKind::AuthTokenMint, message).with_source(err)
        })?;
        // Checked before the token is cached, so a map missing the auth header is a
        // mint failure rather than a cached value that fails every request until the
        // refresh window rolls over.
        token.auth_value()?;

        *self.cached.write().unwrap_or_else(PoisonError::into_inner) = Some(Cached {
            token: token.clone(),
            minted_at: self.clock.elapsed(),
        });
        self.mint_count.fetch_add(1, Ordering::SeqCst);
        Ok(token)
    }

    /// The cached token, or `None` when there is no token or it is stale.
    ///
    /// Cloned out rather than handed back behind the read guard, because the guard cannot be
    /// held across the await in [`Self::token`]'s slow path (see the field's own comment). A
    /// [`ProxyToken`] is a small map, and this is once per request at most.
    fn fresh_token(&self) -> Option<ProxyToken> {
        let cached = self.cached.read().unwrap_or_else(PoisonError::into_inner);
        let entry = cached.as_ref()?;
        // `saturating_sub` because a clock is only promised to be monotonic, and a
        // negative age would otherwise wrap into an enormous one and re-mint on every
        // request.
        let age = self.clock.elapsed().saturating_sub(entry.minted_at);
        if age > self.refresh_after {
            return None;
        }
        Some(entry.token.clone())
    }

    /// Both required headers, built from one token.
    fn headers_from(&self, token: &ProxyToken) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = token
            .headers()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        // The port header is this client's to send, not the token's to carry — but if
        // a future control-plane response includes it, that value wins, because the
        // service knows which ports it scoped the token to and this client only knows
        // which one it was configured with.
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(PROXY_PORT_HEADER))
        {
            headers.push((PROXY_PORT_HEADER.to_string(), self.port.to_string()));
        }
        headers
    }
}

impl fmt::Debug for ProxyAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyAuth")
            .field("port", &self.port)
            .field("refresh_after", &self.refresh_after)
            .field("mint_count", &self.mint_count())
            .field("cached", &self.is_cached())
            .finish()
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Fakes the session tests share. `pub(crate)` so `mod.rs`'s and `exec.rs`'s
    //! tests reach the same clock and the same minter rather than each growing a
    //! near-copy that can drift.

    use super::*;

    /// A clock a test moves by hand, so a sixty-minute boundary costs no wall time
    /// and no simulator.
    #[derive(Debug, Default)]
    pub(crate) struct ManualClock {
        millis: AtomicU64,
    }

    impl ManualClock {
        pub(crate) fn advance(&self, by: Duration) {
            self.millis
                .fetch_add(by.as_millis() as u64, Ordering::SeqCst);
        }
    }

    impl Clock for ManualClock {
        fn elapsed(&self) -> Duration {
            Duration::from_millis(self.millis.load(Ordering::SeqCst))
        }
    }

    /// A minter that answers with a token naming its own sequence number, so a test
    /// can tell a refreshed token from a cached one by the value on the wire.
    #[derive(Debug, Default)]
    pub(crate) struct CountingMinter {
        issued: AtomicU64,
        /// How many of the next mints fail. Drives the retryable-mint scenario.
        failures: AtomicU64,
        /// When set, the token map omits [`PROXY_AUTH_HEADER`] — the TRAP-7 shape a
        /// client that read `authToken` as a string would never notice.
        omit_auth_header: bool,
    }

    impl CountingMinter {
        pub(crate) fn failing(times: u64) -> Self {
            Self {
                failures: AtomicU64::new(times),
                ..Self::default()
            }
        }

        pub(crate) fn without_auth_header() -> Self {
            Self {
                omit_auth_header: true,
                ..Self::default()
            }
        }

        /// The auth value the nth token carries.
        pub(crate) fn value(nth: u64) -> String {
            format!("jwe-{nth}")
        }
    }

    impl TokenMinter for CountingMinter {
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
                if self.omit_auth_header {
                    return Ok(ProxyToken::from_pairs([("X-aws-something-else", "junk")]));
                }
                Ok(ProxyToken::from_pairs([(
                    PROXY_AUTH_HEADER.to_string(),
                    Self::value(nth),
                )]))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{CountingMinter, ManualClock};
    use super::*;
    use crate::error::ErrorKind;

    fn auth(clock: Arc<ManualClock>, refresh_after: Duration) -> ProxyAuth {
        ProxyAuth::with_refresh_after(
            Arc::new(CountingMinter::default()),
            DEFAULT_AGENT_PORT,
            refresh_after,
            clock,
        )
        .expect("the interval is below the ceiling")
    }

    fn value_of(headers: &[(String, String)], name: &str) -> Option<String> {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }

    /// The shipped default has to satisfy the guard the fallible constructor enforces,
    /// which is what lets [`ProxyAuth::new`] be infallible.
    ///
    /// This is also the TRAP-9 falsification at its cheapest: set
    /// `DEFAULT_REFRESH_AFTER` to `MAX_TOKEN_LIFETIME` and this test is red.
    #[test]
    fn the_default_refresh_interval_is_strictly_below_the_service_ceiling() {
        assert!(
            DEFAULT_REFRESH_AFTER < MAX_TOKEN_LIFETIME,
            "refreshing at or after the ceiling puts the expiry inside the window \
             between building the headers and the proxy reading them"
        );
        assert_eq!(MAX_TOKEN_LIFETIME, Duration::from_secs(3600));
        assert_eq!(DEFAULT_REFRESH_AFTER, MAX_TOKEN_LIFETIME / 2);
    }

    /// An interval at the ceiling is refused locally, before any request could carry
    /// an expired token (S2).
    #[test]
    fn a_refresh_interval_at_the_ceiling_is_refused() {
        let err = ProxyAuth::with_refresh_after(
            Arc::new(CountingMinter::default()),
            DEFAULT_AGENT_PORT,
            MAX_TOKEN_LIFETIME,
            Arc::new(ManualClock::default()),
        )
        .expect_err("an interval at the ceiling must be refused");
        assert_eq!(err.kind(), ErrorKind::InvalidArg);
        assert!(
            err.to_string().contains("ceiling"),
            "the message must name the finding: {err}"
        );

        // One second under is accepted, so the guard is a comparison rather than a
        // blanket refusal of anything near an hour.
        assert!(
            ProxyAuth::with_refresh_after(
                Arc::new(CountingMinter::default()),
                DEFAULT_AGENT_PORT,
                MAX_TOKEN_LIFETIME - Duration::from_secs(1),
                Arc::new(ManualClock::default()),
            )
            .is_ok()
        );
    }

    /// Both headers on every set, which is the whole of TRAP-7's send side.
    #[tokio::test]
    async fn every_header_set_carries_both_the_auth_and_the_port_header() {
        let auth = auth(Arc::new(ManualClock::default()), DEFAULT_REFRESH_AFTER);
        let headers = auth.headers().await.expect("the fake minter succeeds");
        assert_eq!(
            value_of(&headers, PROXY_AUTH_HEADER).as_deref(),
            Some(CountingMinter::value(0).as_str())
        );
        assert_eq!(
            value_of(&headers, PROXY_PORT_HEADER).as_deref(),
            Some("9000"),
            "without the port header the proxy answers as if the token were bad"
        );
        assert_eq!(
            headers.len(),
            2,
            "no third header was invented: {headers:?}"
        );
    }

    /// The token is a map, and a map that lacks the auth key is a mint failure rather
    /// than a token.
    ///
    /// The TRAP-7 shape: a client that read `authToken` as a string would put the
    /// stringified map — or nothing — on the wire and see a proxy rejection.
    #[tokio::test]
    async fn a_token_map_without_the_auth_header_is_a_retryable_mint_failure() {
        let auth = ProxyAuth::with_refresh_after(
            Arc::new(CountingMinter::without_auth_header()),
            DEFAULT_AGENT_PORT,
            DEFAULT_REFRESH_AFTER,
            Arc::new(ManualClock::default()),
        )
        .expect("interval accepted");
        let err = auth.headers().await.expect_err("the map has no auth key");
        assert_eq!(err.wire_kind(), Some(WireKind::AuthTokenMint));
        assert!(err.retryable());
        assert!(
            err.to_string().contains(PROXY_AUTH_HEADER),
            "the message must name the missing key: {err}"
        );
        assert_eq!(
            auth.mint_count(),
            0,
            "an unusable token must not be counted as minted or cached"
        );
    }

    /// A second request inside the window reuses the token: minting is per-staleness,
    /// not per-request.
    #[tokio::test]
    async fn a_token_inside_the_refresh_window_is_reused() {
        let clock = Arc::new(ManualClock::default());
        let auth = auth(Arc::clone(&clock), DEFAULT_REFRESH_AFTER);

        let first = auth.headers().await.expect("mints");
        clock.advance(DEFAULT_REFRESH_AFTER - Duration::from_secs(1));
        let second = auth.headers().await.expect("reuses");

        assert_eq!(first, second);
        assert_eq!(auth.mint_count(), 1);
    }

    /// Crossing the refresh window re-mints, and the fresh value reaches the wire.
    ///
    /// The value assertion is the load-bearing half: a client that refreshed its cache
    /// but kept emitting the old header would pass a mint-count assertion and fail
    /// every request.
    #[tokio::test]
    async fn crossing_the_refresh_window_mints_a_new_token_and_sends_it() {
        let clock = Arc::new(ManualClock::default());
        let auth = auth(Arc::clone(&clock), DEFAULT_REFRESH_AFTER);

        let first = auth.headers().await.expect("mints");
        assert_eq!(
            value_of(&first, PROXY_AUTH_HEADER).as_deref(),
            Some(CountingMinter::value(0).as_str())
        );

        clock.advance(DEFAULT_REFRESH_AFTER + Duration::from_secs(1));
        let second = auth.headers().await.expect("re-mints");

        assert_eq!(auth.mint_count(), 2);
        assert_eq!(
            value_of(&second, PROXY_AUTH_HEADER).as_deref(),
            Some(CountingMinter::value(1).as_str()),
            "the refreshed token never reached the headers"
        );
    }

    /// Walks two ceilings' worth of requests and reports the oldest token any of them
    /// carried, plus how many mints it took.
    ///
    /// Constructed with a struct literal rather than through
    /// [`ProxyAuth::with_refresh_after`], deliberately: the constructor refuses an
    /// interval at the ceiling, which is exactly the interval the test below has to
    /// measure. Going around the guard is what makes the property falsifiable — a test
    /// that could only build a *safe* interval could not tell a correct refresh schedule
    /// from a constructor that happened to reject the one bad input.
    async fn walk_two_ceilings(refresh_after: Duration) -> (Duration, u64) {
        let clock = Arc::new(ManualClock::default());
        let auth = ProxyAuth {
            minter: Arc::new(CountingMinter::default()),
            port: DEFAULT_AGENT_PORT,
            refresh_after,
            clock: Arc::clone(&clock) as Arc<dyn Clock>,
            cached: RwLock::new(None),
            mint_lock: tokio::sync::Mutex::new(()),
            mint_count: AtomicU64::new(0),
        };

        // One-minute steps, finer than any refresh boundary in play.
        let mut minted_at = Duration::ZERO;
        let mut oldest = Duration::ZERO;
        for minute in 0..120u64 {
            let now = Duration::from_secs(minute * 60);
            let before = auth.mint_count();
            auth.headers().await.expect("the fake minter succeeds");
            if auth.mint_count() > before {
                minted_at = now;
            }
            oldest = oldest.max(now - minted_at);
            clock.advance(Duration::from_secs(60));
        }
        (oldest, auth.mint_count())
    }

    /// No request ever carries a token past the service ceiling (TRAP-9).
    ///
    /// Stated over the whole walk rather than at one instant: the observable is the age
    /// of the *oldest* token any request presented, which is the only number that can
    /// say a schedule is safe rather than that it happened to be safe at the moment
    /// checked.
    ///
    /// **Guard proof.** The second half is the falsification, and it is why this test
    /// builds a `ProxyAuth` by hand: at a ceiling-width interval the oldest token
    /// presented is over an hour old, so a client that shipped `refresh_after =
    /// MAX_TOKEN_LIFETIME` — the plausible mistake, since it reads as "refresh when the
    /// token expires" — would be caught here. Both halves run, so the test cannot pass
    /// by measuring nothing.
    #[tokio::test]
    async fn a_token_is_never_presented_after_the_service_ceiling() {
        let (oldest, mints) = walk_two_ceilings(DEFAULT_REFRESH_AFTER).await;
        assert!(
            oldest < MAX_TOKEN_LIFETIME,
            "a request carried a token {}s old, past the {}s ceiling",
            oldest.as_secs(),
            MAX_TOKEN_LIFETIME.as_secs()
        );
        assert!(
            MAX_TOKEN_LIFETIME - oldest >= MAX_TOKEN_LIFETIME - DEFAULT_REFRESH_AFTER,
            "the oldest token presented had only {}s of life left",
            (MAX_TOKEN_LIFETIME - oldest).as_secs()
        );
        // Two hours at a thirty-minute window: mints land at minute 0, then at 31, 62,
        // and 93, since staleness is `age > refresh_after` rather than `>=`.
        assert_eq!(mints, 4);

        // The falsification, run rather than described: the exact negation of the
        // assertion above. A token presented at *exactly* the ceiling has zero life
        // left, which is why the safe comparison is strict.
        let (oldest_at_ceiling, _) = walk_two_ceilings(MAX_TOKEN_LIFETIME).await;
        assert!(
            oldest_at_ceiling >= MAX_TOKEN_LIFETIME,
            "refreshing at the ceiling presented a token only {}s old, so this \
             property cannot tell a safe schedule from an unsafe one",
            oldest_at_ceiling.as_secs()
        );
    }

    /// `invalidate` drops the cache, so the next request mints (STATE-8).
    #[tokio::test]
    async fn invalidate_forces_the_next_request_to_mint() {
        let clock = Arc::new(ManualClock::default());
        let auth = auth(Arc::clone(&clock), DEFAULT_REFRESH_AFTER);

        auth.headers().await.expect("mints");
        assert_eq!(auth.mint_count(), 1);
        assert!(auth.is_cached());

        auth.invalidate();
        assert!(!auth.is_cached(), "invalidate did not drop the cache");

        // No clock movement at all: the re-mint is caused by the invalidation, not by
        // the window rolling over.
        let refreshed = auth.headers().await.expect("re-mints");
        assert_eq!(auth.mint_count(), 2);
        assert_eq!(
            value_of(&refreshed, PROXY_AUTH_HEADER).as_deref(),
            Some(CountingMinter::value(1).as_str())
        );
    }

    /// A mint failure is retryable, and the retry is a plain second call.
    #[tokio::test]
    async fn a_failed_mint_is_retryable_and_the_next_attempt_succeeds() {
        let auth = ProxyAuth::with_refresh_after(
            Arc::new(CountingMinter::failing(1)),
            DEFAULT_AGENT_PORT,
            DEFAULT_REFRESH_AFTER,
            Arc::new(ManualClock::default()),
        )
        .expect("interval accepted");

        let err = auth.headers().await.expect_err("the first mint fails");
        assert_eq!(err.kind(), ErrorKind::Retryable);
        assert_eq!(err.wire_kind(), Some(WireKind::AuthTokenMint));
        assert_eq!(auth.mint_count(), 0);

        let headers = auth.headers().await.expect("the second mint succeeds");
        assert_eq!(auth.mint_count(), 1);
        assert!(value_of(&headers, PROXY_AUTH_HEADER).is_some());
    }

    /// Concurrent requests on a cold cache mint once, not once each.
    #[tokio::test]
    async fn concurrent_requests_on_a_cold_cache_mint_once() {
        let auth = Arc::new(auth(
            Arc::new(ManualClock::default()),
            DEFAULT_REFRESH_AFTER,
        ));
        let mut sets = Vec::new();
        for _ in 0..8 {
            sets.push(Arc::clone(&auth).headers_owned());
        }
        let results = futures_util::future::join_all(sets).await;
        for result in &results {
            assert!(result.is_ok(), "{result:?}");
        }
        assert_eq!(
            auth.mint_count(),
            1,
            "eight concurrent requests burned more than one control-plane call"
        );
    }

    impl ProxyAuth {
        /// `headers()` with an owned receiver, so a test can hold several futures at
        /// once without borrowing one `ProxyAuth` eight times.
        #[cfg(test)]
        async fn headers_owned(self: Arc<Self>) -> Result<Vec<(String, String)>, Error> {
            self.headers().await
        }
    }

    // ── the WebSocket handshake credentials (`subprotocols`) ──────────────────
    //
    // Four tests. The first pins the wire format, because the platform matches these strings
    // exactly and a client that got a separator wrong would fail a handshake with no
    // diagnostic that names the subprotocol. The rest are the same three properties the
    // header path already has — one mint, refresh at the window, and an unusable token map is
    // a retryable failure — asserted through *this* accessor, because "headers() mints
    // correctly" is not a statement about a second door onto the same cache.

    /// **The three subprotocols are exactly the platform's strings, in order.**
    ///
    /// The port is substituted rather than fixed, and the base value carries no suffix. Every
    /// separator is asserted with a literal on purpose: the platform matches these by exact
    /// string, so a `:` where a `.` belongs, or the port and auth values transposed, is a
    /// handshake failure whose error message names neither.
    ///
    /// **Guard proof.** Change the separator in `WS_PORT_SUBPROTOCOL_PREFIX` from `.` to `-`
    /// and the third assertion is red; swap the order of the last two array elements and the
    /// order assertion is red. Both verified.
    #[tokio::test]
    async fn the_subprotocols_are_the_platforms_three_exact_strings_in_order() {
        let auth = auth(Arc::new(ManualClock::default()), DEFAULT_REFRESH_AFTER);
        let offered = auth
            .subprotocols(8080)
            .await
            .expect("the fake minter mints");

        assert_eq!(
            offered,
            [
                "lambda-microvms".to_string(),
                format!(
                    "lambda-microvms.authentication.{}",
                    CountingMinter::value(0)
                ),
                "lambda-microvms.port.8080".to_string(),
            ],
            "the handshake offered something the proxy does not match"
        );
        // The port is read from the argument and not from the instance, which is what lets one
        // session mint for a workload port it was not built against.
        assert_eq!(auth.port(), DEFAULT_AGENT_PORT);
        assert!(
            offered[2].ends_with("8080"),
            "the port was taken from the instance rather than the argument: {}",
            offered[2]
        );
    }

    /// **A handshake reuses the cached token rather than minting a second one.**
    ///
    /// The property that says `subprotocols` goes through the existing cache: three calls,
    /// mixed with a header call, and the control plane is touched once. A second token path
    /// would pass every format assertion above and fail here.
    ///
    /// **Guard proof.** Mint directly in `subprotocols` (`self.minter.mint().await`) instead
    /// of calling `self.token()` and the mint count reads 4. Verified.
    #[tokio::test]
    async fn a_handshake_reuses_the_cached_token_rather_than_minting_a_second_one() {
        let clock = Arc::new(ManualClock::default());
        let auth = auth(Arc::clone(&clock), DEFAULT_REFRESH_AFTER);

        let first = auth.subprotocols(9000).await.expect("mints");
        clock.advance(DEFAULT_REFRESH_AFTER - Duration::from_secs(1));
        let second = auth.subprotocols(9000).await.expect("reuses");
        let headers = auth.headers().await.expect("reuses");
        let third = auth.subprotocols(3000).await.expect("reuses");

        assert_eq!(first, second, "the same cached token produced two values");
        assert_eq!(
            auth.mint_count(),
            1,
            "the handshake path burned its own control-plane call, so TRAP-9 is now two \
             schedules to get right instead of one"
        );
        // The same token reached both surfaces, which is what "one cache" means observably.
        assert_eq!(
            value_of(&headers, PROXY_AUTH_HEADER).as_deref(),
            Some(CountingMinter::value(0).as_str())
        );
        assert_eq!(third[1], first[1], "the auth value differed across ports");
        assert_eq!(third[2], "lambda-microvms.port.3000");
    }

    /// **A handshake past the refresh window carries the fresh token.**
    ///
    /// The lazy-refresh half of TRAP-9, on the handshake path: a connection opened after the
    /// window has rolled over must carry a token minted now, because a WebSocket lives longer
    /// than a request and a stale one is rejected as a bad token.
    ///
    /// **Guard proof.** Have `subprotocols` read `self.cached` directly instead of going
    /// through `token()` and the second value still reads `jwe-0`. Verified.
    #[tokio::test]
    async fn a_handshake_after_the_refresh_window_carries_the_freshly_minted_token() {
        let clock = Arc::new(ManualClock::default());
        let auth = auth(Arc::clone(&clock), DEFAULT_REFRESH_AFTER);

        let first = auth.subprotocols(9000).await.expect("mints");
        assert!(first[1].ends_with(&CountingMinter::value(0)));

        clock.advance(DEFAULT_REFRESH_AFTER + Duration::from_secs(1));
        let second = auth.subprotocols(9000).await.expect("re-mints");

        assert_eq!(auth.mint_count(), 2);
        assert_eq!(
            second[1],
            format!("{WS_AUTH_SUBPROTOCOL_PREFIX}{}", CountingMinter::value(1)),
            "the handshake offered the expired token, which the proxy rejects as if it were \
             the wrong token"
        );
    }

    /// **A token map with no auth key is a retryable mint failure here too, and no handshake
    /// offers an empty credential.**
    ///
    /// The TRAP-7 shape on the handshake path. The failure that matters is not the error type
    /// but its alternative: `format!("{PREFIX}{}", "")` is a *syntactically valid*
    /// subprotocol, so a client that skipped the check would offer
    /// `lambda-microvms.authentication.` and get a rejection that says nothing about an empty
    /// token.
    ///
    /// **Guard proof.** Removing *both* `auth_value` checks — the one in `token()`'s mint path
    /// and the one in `subprotocols` — makes this red: the call succeeds and the middle value
    /// is the bare prefix. Verified. Removing only the `subprotocols` one leaves this green,
    /// which is the honest measurement: `token()`'s pre-cache check is the guard that fires
    /// today, and `subprotocols` carries the same check for the reason its own comment gives.
    #[tokio::test]
    async fn a_token_map_without_the_auth_key_fails_the_handshake_rather_than_offering_nothing() {
        let auth = ProxyAuth::with_refresh_after(
            Arc::new(CountingMinter::without_auth_header()),
            DEFAULT_AGENT_PORT,
            DEFAULT_REFRESH_AFTER,
            Arc::new(ManualClock::default()),
        )
        .expect("interval accepted");

        let err = auth
            .subprotocols(9000)
            .await
            .expect_err("the map has no auth key");
        assert_eq!(err.wire_kind(), Some(WireKind::AuthTokenMint));
        assert!(err.retryable());
        assert!(
            err.to_string().contains(PROXY_AUTH_HEADER),
            "the message must name the missing key: {err}"
        );
    }

    /// **A subprotocol list is a credential, so nothing this module stores can print one.**
    ///
    /// Not a claim about the strings — they are returned, and a caller can log anything it is
    /// handed. The claim is about the *types*: after a handshake has been built, the
    /// `ProxyAuth` that built it and the `ProxyToken` it cached both render without the value,
    /// so a log line that formats a session or an error chain carries no token.
    ///
    /// **Guard proof.** Add the auth value to either `Debug` impl and this is red. Verified by
    /// adding `.field("token", &offered[1])` to `ProxyAuth`'s.
    #[tokio::test]
    async fn nothing_that_survives_a_handshake_can_print_the_credential() {
        let auth = ProxyAuth::with_refresh_after(
            Arc::new(CountingMinter::default()),
            DEFAULT_AGENT_PORT,
            DEFAULT_REFRESH_AFTER,
            Arc::new(ManualClock::default()),
        )
        .expect("interval accepted");
        let offered = auth.subprotocols(9000).await.expect("mints");
        let credential = CountingMinter::value(0);
        assert!(
            offered[1].contains(&credential),
            "the test is measuring nothing unless the value really is in the list"
        );

        let rendered = format!("{auth:?}");
        assert!(
            !rendered.contains(&credential),
            "the auth token reached ProxyAuth's Debug: {rendered}"
        );
        assert!(
            rendered.contains("mint_count"),
            "the Debug still has to be useful: {rendered}"
        );
    }

    // ── the per-port header pair (`headers_for_port`) ─────────────────────────

    /// **`headers_for_port` answers both headers for a port this instance was not built
    /// with, off the same cache.**
    ///
    /// The `getPortEndpoint() -> {url, headers}` shape. Two properties in one test because
    /// they are the same claim from both sides: the *requested* port reaches the header, and
    /// no second token is minted to get it there.
    ///
    /// **Guard proof.** Return `self.headers()` from `headers_for_port` (the plausible
    /// mistake, since the instance already has a port) and the port assertion reads `9000`.
    /// Verified.
    #[tokio::test]
    async fn headers_for_a_port_name_that_port_and_reuse_the_cached_token() {
        let auth = auth(Arc::new(ManualClock::default()), DEFAULT_REFRESH_AFTER);

        auth.headers().await.expect("mints for the session's port");
        let for_workload = auth.headers_for_port(8080).await.expect("reuses");

        assert_eq!(
            value_of(&for_workload, PROXY_PORT_HEADER).as_deref(),
            Some("8080"),
            "the request would target the session's port rather than the caller's"
        );
        assert_eq!(
            value_of(&for_workload, PROXY_AUTH_HEADER).as_deref(),
            Some(CountingMinter::value(0).as_str())
        );
        assert_eq!(
            for_workload.len(),
            2,
            "no third header was invented and none was duplicated: {for_workload:?}"
        );
        assert_eq!(auth.mint_count(), 1, "the second port burned a second mint");
    }

    /// **A requested port wins over one the token map carries, and there is exactly one port
    /// header on the wire.**
    ///
    /// The deliberate divergence from [`ProxyAuth::headers`], which defers to the map. Here a
    /// caller named a port, and answering with a different one would send a request the caller
    /// cannot predict. The duplicate is the sharper half: HTTP would carry both values and the
    /// proxy would read whichever it saw first, so the behaviour would depend on header
    /// ordering.
    ///
    /// **Guard proof.** Drop the `.filter(..)` in `headers_for_port` and the length assertion
    /// reads 3 with two port headers present. Verified.
    #[tokio::test]
    async fn a_requested_port_replaces_one_the_token_map_carried() {
        struct PortCarryingMinter;
        impl TokenMinter for PortCarryingMinter {
            fn mint(&self) -> BoxFuture<'_, Result<ProxyToken, Error>> {
                Box::pin(async move {
                    Ok(ProxyToken::from_pairs([
                        (PROXY_AUTH_HEADER, "jwe-from-control"),
                        (PROXY_PORT_HEADER, "9000"),
                    ]))
                })
            }
        }

        let auth = ProxyAuth::with_refresh_after(
            Arc::new(PortCarryingMinter),
            DEFAULT_AGENT_PORT,
            DEFAULT_REFRESH_AFTER,
            Arc::new(ManualClock::default()),
        )
        .expect("interval accepted");

        // The header path defers to the map, which is its own documented rule.
        let session_headers = auth.headers().await.expect("mints");
        assert_eq!(
            value_of(&session_headers, PROXY_PORT_HEADER).as_deref(),
            Some("9000")
        );

        let for_workload = auth.headers_for_port(8080).await.expect("reuses");
        assert_eq!(
            for_workload
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case(PROXY_PORT_HEADER))
                .count(),
            1,
            "two port headers went out, so which port is targeted depends on ordering: \
             {for_workload:?}"
        );
        assert_eq!(
            value_of(&for_workload, PROXY_PORT_HEADER).as_deref(),
            Some("8080"),
            "the caller's port lost to the token map's"
        );
    }

    /// A token's `Debug` names its keys and never its values, so a logged session does
    /// not log a credential.
    #[test]
    fn a_proxy_token_debug_does_not_print_the_credential() {
        let token = ProxyToken::from_pairs([(PROXY_AUTH_HEADER, "eyJhbGciOi-secret")]);
        let rendered = format!("{token:?}");
        assert!(rendered.contains(PROXY_AUTH_HEADER), "{rendered}");
        assert!(
            !rendered.contains("secret"),
            "the token value reached a Debug string: {rendered}"
        );
    }

    /// Header lookup is case-insensitive, because the keys come from a service
    /// response rather than from this crate.
    #[test]
    fn the_auth_header_is_found_whatever_case_the_service_used() {
        let token = ProxyToken::from_pairs([("x-AWS-Proxy-AUTH", "jwe")]);
        assert_eq!(token.auth_value().expect("found"), "jwe");
        assert!(!token.is_empty());
    }

    /// The control plane's token converts into this one with both headers intact.
    ///
    /// The two lanes independently defined `PROXY_AUTH_HEADER` and `PROXY_PORT_HEADER`,
    /// and this is what makes the duplication safe rather than a latent divergence: the
    /// conversion goes through the *other* module's spelling, so if either lane ever
    /// changed a header name the port assertion here would fail. Cheaper than merging the
    /// constants, which would mean one lane editing the other's file.
    #[test]
    fn a_control_plane_token_converts_with_both_headers_intact() {
        let mut map = std::collections::BTreeMap::new();
        map.insert(
            crate::control::microvm::PROXY_AUTH_HEADER.to_string(),
            "jwe-from-control".to_string(),
        );
        let minted =
            crate::control::ProxyToken::from_map(&map, 9000).expect("the map has the auth key");

        let converted: ProxyToken = minted.into();
        assert_eq!(
            converted.auth_value().expect("the auth value survived"),
            "jwe-from-control"
        );
        assert_eq!(
            converted.get(PROXY_PORT_HEADER),
            Some("9000"),
            "the port header did not survive the conversion, so every request built \
             from a control-plane token would be rejected as if the token were bad"
        );
    }
}
