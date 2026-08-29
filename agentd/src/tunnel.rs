// SPDX-License-Identifier: Apache-2.0
//! The TCP relay: a WebSocket in, a guest TCP connection out.
//!
//! What makes `microvm tunnel` possible. The endpoint proxy carries HTTPS and WebSocket
//! and has **no CONNECT method**, so arbitrary TCP cannot be tunnelled as a protocol the
//! proxy understands — it has to ride inside WebSocket frames. Measured 2026-08-29
//! (`docs/PLATFORM.md`): binary frames survive a port-scoped token byte-exact in both
//! directions, including `0x00`/`0xFF` payloads and the extended-length header form. That
//! measurement is the whole premise of this module, and it was taken before this code was
//! written rather than assumed by it.
//!
//! # This route is bearer-authed, and that is the security boundary
//!
//! It sits on the control router, so [`crate::auth::require_token`] runs before the
//! upgrade is granted. That matters more here than on any other route: a relay reachable
//! without the agent token would let **the workload inside the VM** open connections
//! through the daemon, and the daemon is the one process the platform's own trust model
//! treats as distinct from the workload. The workload already has loopback access to its
//! own ports, so this would not grant it new reach — but it would let it reach them
//! *through the daemon's identity*, which is exactly the confusion `docs/TRUST.md` exists
//! to prevent.
//!
//! # A frame is a byte range, not a message
//!
//! TCP has no message boundaries, so neither does this relay: a frame carries whatever
//! bytes were available, and a caller must not read framing into them. The relay never
//! coalesces or splits deliberately, but it also promises nothing about how a stream
//! divides across frames — which is the same contract TCP itself offers.
//!
//! # What this module does not do
//!
//! **No multiplexing.** One WebSocket carries one TCP connection. A mux protocol would
//! need stream ids, flow control per stream, and a close handshake per stream — three
//! things the platform already gives us per connection, and three more places to have a
//! bug. `microvm tunnel` opens N WebSockets for N local connections.
//!
//! **No egress.** The relay dials `127.0.0.1` only, never a hostname and never another
//! host. A relay that resolved names would be an open proxy sitting inside the VM,
//! reachable by anyone holding the agent token, and the token is delivered to the guest at
//! launch. Loopback-only means the worst a stolen token buys is what the token already
//! buys everywhere else in this API: access to this VM.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

use crate::state::AppState;

/// How long the dial to the guest port may take.
///
/// Short, because the target is loopback: a connect that has not completed in a second is
/// a closed port whose RST was lost, not a slow server. The distinction matters because
/// the caller is holding a WebSocket open waiting for an answer.
const DIAL_TIMEOUT: Duration = Duration::from_secs(2);

/// Bytes read from the guest per relayed frame.
///
/// 64 KiB matches the read buffer a plain `copy` would use and sits well under any
/// WebSocket frame limit in the path. Larger frames would mean fewer syscalls and more
/// latency before the first byte reaches the caller; this is the usual balance point.
const RELAY_CHUNK_BYTES: usize = 64 * 1024;

/// The largest single Noise message, from the Noise spec's own framing.
///
/// 65535 because a Noise transport message is length-prefixed with two bytes. Spelled here
/// rather than read from `snow::constants`, which is a private module: a buffer sized smaller
/// than this makes `read_message` fail as though the *peer* were wrong, so the number matters
/// and a wrong one is expensive to diagnose. The `const` block in this module's tests asserts
/// the chunk size leaves room for the authentication tag underneath it.
const NOISE_MAX_MESSAGE_BYTES: usize = 65535;

/// The authentication tag every Noise transport message carries, in bytes.
///
/// ChaCha20-Poly1305's tag. It shares the [`NOISE_MAX_MESSAGE_BYTES`] ceiling with the
/// ciphertext, which is why the verified path reads a smaller chunk than the plain one.
const NOISE_TAG_BYTES: usize = 16;

/// The verified path's chunk has to leave room for the authentication tag.
///
/// At module scope rather than in the test module, so it guards every build: a Noise
/// transport message carries ciphertext *and* the tag inside one 65535-byte ceiling, and
/// `write_message` answers a plaintext that does not fit with `Error::Input` rather than
/// fragmenting it. So a chunk edited up to 64 KiB would not slow the tunnel down — it would
/// break every send on it, at runtime, on the byte path. This makes that edit fail to
/// compile instead.
const _: () = {
    assert!(
        handshake::NOISE_CHUNK_BYTES + NOISE_TAG_BYTES <= NOISE_MAX_MESSAGE_BYTES,
        "a verified chunk plus its tag must fit one Noise message, or every send fails"
    );
    assert!(
        handshake::NOISE_CHUNK_BYTES >= 1500,
        "a verified chunk below an MTU frames per packet"
    );
};

// The query shape and the close codes are the protocol crate's (ARCH-2): the client drives
// this route, so a code spelled independently here could drift from the one it reads.
pub use protocol::tunnel::{TunnelQuery, close};

/// Upgrades to a WebSocket and relays it to `127.0.0.1:<port>` in the guest.
///
/// The dial happens **after** the upgrade rather than before it, and that is deliberate: a
/// pre-upgrade dial failure would have to be reported as an HTTP status, and on the
/// endpoint path every WebSocket failure the caller can observe is close code 1006 with no
/// reason — so an HTTP 502 here would be invisible. Upgrading first and closing with
/// [`close::NO_LISTENER`] gives the caller a code it can actually read.
/// # The marker subprotocol is echoed when the caller offers it
///
/// Only matters off the endpoint path, and it is the difference between a tunnel that works
/// and one that cannot handshake. Through the proxy the caller's three values never arrive —
/// the proxy consumes them and supplies `lambda-microvms` to the client on the guest's behalf
/// (measured, `docs/PLATFORM.md`) — so there is nothing here to echo and nothing to do.
/// Reached **directly**, the values do arrive, and RFC 6455 clients refuse a handshake that
/// offered subprotocols and got none back: tungstenite fails it with "Server sent no
/// subprotocol". Echoing the marker when it is offered makes the direct path behave like the
/// proxied one, which is what lets one client drive both.
///
/// `protocols` picks the first offered value it recognises, so the two credential-bearing
/// values are never selected — and it selects nothing at all when the caller offered nothing.
pub async fn open(
    State(state): State<AppState>,
    Query(query): Query<TunnelQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let port = query.port;
    // Read before the upgrade, because after it there is no state to reach: the closure below
    // owns everything the relay gets. `None` here with `identity` asked-for is the refusal
    // case, and it is decided inside the closure so it arrives as a close code rather than an
    // HTTP status — on the endpoint path an HTTP status would be invisible.
    let material = state.tunnel_identity();
    let wanted = query.identity;
    upgrade
        .protocols([protocol::tunnel::WS_MARKER_SUBPROTOCOL])
        .on_upgrade(move |socket| async move {
            if wanted {
                verified_relay(socket, port, material).await;
            } else {
                relay(socket, port).await;
            }
        })
}

/// Runs the Noise KK handshake, then relays the guest connection inside it.
///
/// The handshake happens **before** the dial, so a caller whose identity is refused never
/// causes a connection to a guest service. That ordering is deliberate: the cheapest refusal
/// is one that touches nothing, and a guest server that logged a connection from a rejected
/// caller would be misleading evidence.
async fn verified_relay(socket: WebSocket, port: u16, material: crate::tunnel_identity::Shared) {
    let Some(material) = material else {
        // The caller asked for proof and this VM has no key. Refused rather than downgraded:
        // a caller that received an unverified tunnel here would believe a proof it never
        // got, which is worse than either a refusal or never asking.
        tracing::info!(
            port,
            "identity tunnel refused: this VM was launched without a seed"
        );
        close_with(
            socket,
            close::NO_IDENTITY,
            "this VM was launched without an identity seed; the seed is delivered at launch \
             and cannot be added to a running VM",
        )
        .await;
        return;
    };

    let responder = match material.responder() {
        Ok(responder) => responder,
        Err(error) => {
            // A build failure means the stored material is unusable, which the run hook
            // should have caught. Reported as a relay failure rather than an identity
            // refusal: the caller's key is not what went wrong.
            tracing::warn!(port, %error, "could not build the identity responder");
            close_with(
                socket,
                close::RELAY_FAILED,
                "the daemon could not build its identity responder",
            )
            .await;
            return;
        }
    };

    let mut noise = match handshake::respond(socket, responder).await {
        Ok(session) => session,
        Err(handshake::Failed { socket, error }) => {
            // One code for a wrong pin and a wrong caller, because under KK the daemon
            // genuinely cannot tell them apart — both statics are mixed into the handshake
            // hash, so both arrive as a decryption failure. A code that claimed to
            // distinguish them would be guessing.
            tracing::info!(port, %error, "identity handshake refused");
            // Short on purpose: RFC 6455 caps a close frame's payload at 125 bytes, and a
            // longer reason makes tungstenite fail the whole read as ControlFrameTooBig — the
            // caller then sees a transport error instead of the refusal code, which is the
            // exact diagnostic this code exists to deliver. The full sentence lives in
            // `close::explanation`, which the client renders from the code alone.
            close_with(
                *socket,
                close::IDENTITY_REFUSED,
                "identity handshake refused: wrong pin or wrong caller",
            )
            .await;
            return;
        }
    };

    // From here the caller is proved, so the dial and the pump are the plain relay's — except
    // every frame passes through the Noise session.
    let stream = match dial(port).await {
        Ok(stream) => stream,
        Err(refusal) => {
            noise.close(refusal.code, &refusal.reason).await;
            return;
        }
    };
    tracing::info!(port, "verified tunnel established");
    pump(noise, stream, port).await;
}

/// Dials the guest port and pumps bytes until either side ends.
async fn relay(socket: WebSocket, port: u16) {
    let mut channel = Plain(socket);
    let stream = match dial(port).await {
        Ok(stream) => stream,
        Err(refusal) => {
            channel.close(refusal.code, &refusal.reason).await;
            return;
        }
    };
    tracing::info!(port, "tunnel established");
    pump(channel, stream, port).await;
}

/// Why a dial did not produce a guest connection, as the close frame the caller will see.
struct Refusal {
    code: u16,
    reason: String,
}

/// Connects to `127.0.0.1:<port>` in the guest, or reports the refusal to relay back.
///
/// Shared by the plain and the verified paths so the two cannot disagree about what a dead
/// port looks like. Loopback only, never a hostname: see the module docs on egress.
async fn dial(port: u16) -> Result<TcpStream, Refusal> {
    if port == 0 {
        return Err(Refusal {
            code: close::BAD_PORT,
            reason: "port 0 cannot be dialled; name the guest port to relay to".to_string(),
        });
    }

    let target = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let stream = match tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect(target)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            tracing::info!(port, %error, "tunnel dial refused");
            return Err(Refusal {
                code: close::NO_LISTENER,
                reason: format!("nothing is listening on 127.0.0.1:{port} in the guest"),
            });
        }
        Err(_) => {
            tracing::info!(port, "tunnel dial timed out");
            return Err(Refusal {
                code: close::NO_LISTENER,
                reason: format!("127.0.0.1:{port} did not accept within {DIAL_TIMEOUT:?}"),
            });
        }
    };

    // `set_nodelay` because the payload is somebody else's protocol: Nagle would batch a
    // small request with whatever came next, and an interactive protocol (a database
    // handshake, a debugger step) would stall waiting for a peer that is waiting for it.
    if let Err(error) = stream.set_nodelay(true) {
        tracing::warn!(port, %error, "could not disable Nagle on the guest connection");
    }
    Ok(stream)
}

/// What arrived from the caller's side of a channel.
enum Inbound {
    /// Bytes for the guest.
    Bytes(Vec<u8>),
    /// The caller finished sending, or the transport ended.
    Closed,
    /// The transport failed; no close frame can be delivered.
    Failed(String),
}

/// A caller-facing byte channel: either a bare WebSocket or one inside a Noise session.
///
/// A trait rather than two copies of the pump loop, because the loop is where the half-close
/// and backpressure semantics live and those must not diverge between a verified tunnel and a
/// plain one — a bug fixed in one copy and not the other is the failure mode this avoids.
trait Channel {
    /// The next bytes from the caller.
    fn recv(&mut self) -> impl std::future::Future<Output = Inbound> + Send;
    /// Sends bytes to the caller. `false` means the transport is gone.
    fn send(&mut self, bytes: &[u8]) -> impl std::future::Future<Output = bool> + Send;
    /// Sends a close frame carrying `code` and `reason`, then drops the channel.
    fn close(&mut self, code: u16, reason: &str) -> impl std::future::Future<Output = ()> + Send;
    /// The largest payload one send may carry.
    ///
    /// A method rather than a constant because the Noise channel's ceiling is lower: its 16-byte
    /// authentication tag has to fit inside the same 65535-byte Noise message as the plaintext,
    /// so a 64 KiB read would be refused by `write_message` rather than fragmented for us.
    fn chunk_bytes(&self) -> usize;
}

/// The layer-2 channel: bytes are WebSocket binary frames, unencrypted past the proxy.
struct Plain(WebSocket);

impl Channel for Plain {
    async fn recv(&mut self) -> Inbound {
        match self.0.recv().await {
            // Binary is the contract. Text is accepted rather than refused because a caller
            // that sent it meant those bytes — refusing would be a second failure mode for no
            // gain — but nothing here produces text.
            Some(Ok(Message::Binary(bytes))) => Inbound::Bytes(bytes.to_vec()),
            Some(Ok(Message::Text(text))) => Inbound::Bytes(text.as_bytes().to_vec()),
            // A caller's half-close means "I have sent everything". The pump shuts the guest's
            // write half rather than dropping the connection, which is what lets a
            // request/response protocol get its response.
            Some(Ok(Message::Close(_))) | None => Inbound::Closed,
            // Ping, pong, and continuation frames are axum's to handle; nothing to relay.
            Some(Ok(_)) => Inbound::Bytes(Vec::new()),
            Some(Err(error)) => Inbound::Failed(error.to_string()),
        }
    }

    async fn send(&mut self, bytes: &[u8]) -> bool {
        self.0
            .send(Message::Binary(bytes.to_vec().into()))
            .await
            .is_ok()
    }

    async fn close(&mut self, code: u16, reason: &str) {
        let frame = axum::extract::ws::CloseFrame {
            code,
            reason: reason.to_string().into(),
        };
        let _ = self.0.send(Message::Close(Some(frame))).await;
    }

    fn chunk_bytes(&self) -> usize {
        RELAY_CHUNK_BYTES
    }
}

/// Relays between a caller channel and a guest connection until either side ends.
///
/// Generic over the channel so a verified tunnel and a plain one share one loop. One task per
/// direction would need a shared sink; a select loop keeps the channel owned here, which is
/// what lets the close frame below be the last thing written.
async fn pump<C: Channel>(mut channel: C, stream: TcpStream, port: u16) {
    let (mut guest_read, mut guest_write) = stream.into_split();
    let mut buffer = vec![0_u8; channel.chunk_bytes()];

    let outcome = loop {
        tokio::select! {
            inbound = channel.recv() => match inbound {
                Inbound::Bytes(bytes) => {
                    if bytes.is_empty() {
                        continue;
                    }
                    if guest_write.write_all(&bytes).await.is_err() {
                        break Ended::GuestClosed;
                    }
                }
                Inbound::Closed => break Ended::CallerClosed,
                Inbound::Failed(error) => {
                    tracing::info!(port, %error, "tunnel transport error");
                    break Ended::TransportFailed;
                }
            },
            read = guest_read.read(&mut buffer) => match read {
                Ok(0) => break Ended::GuestClosed,
                Ok(count) => {
                    if !channel.send(&buffer[..count]).await {
                        break Ended::TransportFailed;
                    }
                }
                Err(error) => {
                    tracing::info!(port, %error, "tunnel guest read failed");
                    break Ended::RelayFailed;
                }
            },
        }
    };

    match outcome {
        // A clean end on either side is code 1000: the tunnel did its job, and a caller
        // that treated a finished connection as an error would retry a completed request.
        Ended::CallerClosed | Ended::GuestClosed => {
            channel.close(close::NORMAL, "").await;
        }
        Ended::RelayFailed => {
            channel
                .close(close::RELAY_FAILED, "the guest connection failed mid-relay")
                .await;
        }
        // Nothing to send: the transport is what failed.
        Ended::TransportFailed => {}
    }
    tracing::info!(port, "tunnel closed");
}

/// The Noise KK handshake over WebSocket frames, and the encrypted channel it yields.
///
/// Two messages, one each way, which is what `KK` costs: the initiator's first message and
/// the responder's reply. Both carry empty payloads — there is nothing to say beyond proving
/// possession of a key, and the guest port already travelled in the query string.
mod handshake {
    use axum::extract::ws::{Message, WebSocket};

    use super::{Channel, Inbound};

    /// A handshake that did not complete, with the socket back so a close code can be sent.
    ///
    /// The socket is returned rather than dropped because the close code *is* the diagnostic on
    /// this path: every platform-side WebSocket failure is 1006 with no reason, so a dropped
    /// socket would tell the caller nothing about why it was refused.
    ///
    /// Boxed because this struct rides the `Err` side of `respond`'s `Result`, and a
    /// `WebSocket` by value puts ~400 bytes into every return — clippy's `result_large_err`
    /// (1.98, the CI toolchain) refuses that, correctly: the happy path pays for the failure
    /// shape on every call.
    pub struct Failed {
        pub socket: Box<WebSocket>,
        pub error: String,
    }

    impl Failed {
        fn new(socket: WebSocket, error: impl Into<String>) -> Self {
            Self {
                socket: Box::new(socket),
                error: error.into(),
            }
        }
    }

    /// Completes the responder side, returning the transport-mode channel.
    pub async fn respond(
        mut socket: WebSocket,
        mut responder: snow::HandshakeState,
    ) -> Result<Noise, Failed> {
        // Sized to the Noise ceiling rather than to the handshake's actual length, because a
        // buffer smaller than a message makes `read_message` fail as though the peer were
        // wrong. The handshake messages here are under 100 bytes.
        let mut scratch = vec![0_u8; super::NOISE_MAX_MESSAGE_BYTES];

        let first = match socket.recv().await {
            Some(Ok(Message::Binary(bytes))) => bytes,
            Some(Ok(Message::Close(_))) | None => {
                return Err(Failed::new(
                    socket,
                    "the caller closed before sending a handshake",
                ));
            }
            Some(Ok(_)) => {
                return Err(Failed::new(
                    socket,
                    "the handshake must arrive as a binary frame",
                ));
            }
            Some(Err(error)) => {
                return Err(Failed::new(socket, error.to_string()));
            }
        };

        // The refusal that matters: under KK both static keys are mixed into the handshake
        // hash, so a caller pinning the wrong VM or holding the wrong host key fails *here*,
        // in a decryption that cannot be skipped by a verifier that forgot a check.
        if let Err(error) = responder.read_message(&first, &mut scratch) {
            return Err(Failed::new(socket, error.to_string()));
        }

        let written = match responder.write_message(&[], &mut scratch) {
            Ok(written) => written,
            Err(error) => {
                return Err(Failed::new(socket, error.to_string()));
            }
        };
        if socket
            .send(Message::Binary(scratch[..written].to_vec().into()))
            .await
            .is_err()
        {
            return Err(Failed::new(socket, "the caller went away mid-handshake"));
        }

        match responder.into_transport_mode() {
            Ok(transport) => Ok(Noise {
                socket,
                transport,
                scratch,
            }),
            Err(error) => Err(Failed::new(socket, error.to_string())),
        }
    }

    /// Bytes read from the guest per Noise message.
    ///
    /// Below the plain relay's 64 KiB on purpose, and the reason is arithmetic rather than
    /// taste: a Noise transport message carries the ciphertext *and* a 16-byte authentication
    /// tag inside one 65535-byte ceiling, so a 65536-byte plaintext is refused outright by
    /// `write_message` — it returns `Error::Input` rather than fragmenting. 32 KiB leaves the
    /// tag room with margin and keeps a comfortable power of two.
    pub const NOISE_CHUNK_BYTES: usize = 32 * 1024;

    /// A caller channel whose frames are Noise transport messages.
    ///
    /// So the bytes the endpoint proxy carries are ciphertext: the identity proof and
    /// end-to-end confidentiality arrive together, because the same handshake that proved the
    /// far end also keyed the cipher.
    pub struct Noise {
        socket: WebSocket,
        transport: snow::TransportState,
        /// One reusable buffer, sized to the Noise ceiling. Reused rather than allocated per
        /// frame because this is the hot path of every byte the tunnel carries.
        scratch: Vec<u8>,
    }

    impl Channel for Noise {
        async fn recv(&mut self) -> Inbound {
            let frame = match self.socket.recv().await {
                Some(Ok(Message::Binary(bytes))) => bytes,
                Some(Ok(Message::Close(_))) | None => return Inbound::Closed,
                // Text is refused here, unlike on the plain channel: a text frame cannot be a
                // Noise message, so passing its bytes through would hand the guest plaintext
                // on a connection whose whole promise is that it is not.
                Some(Ok(Message::Text(_))) => {
                    return Inbound::Failed(
                        "a verified tunnel carries Noise messages in binary frames".to_string(),
                    );
                }
                Some(Ok(_)) => return Inbound::Bytes(Vec::new()),
                Some(Err(error)) => return Inbound::Failed(error.to_string()),
            };
            match self.transport.read_message(&frame, &mut self.scratch) {
                Ok(count) => Inbound::Bytes(self.scratch[..count].to_vec()),
                // A frame that does not authenticate is not a protocol nicety to work around:
                // it is a forged or corrupted frame, and continuing would relay attacker bytes
                // into the guest.
                Err(error) => {
                    Inbound::Failed(format!("a tunnel frame did not authenticate: {error}"))
                }
            }
        }

        async fn send(&mut self, bytes: &[u8]) -> bool {
            let Ok(count) = self.transport.write_message(bytes, &mut self.scratch) else {
                return false;
            };
            self.socket
                .send(Message::Binary(self.scratch[..count].to_vec().into()))
                .await
                .is_ok()
        }

        async fn close(&mut self, code: u16, reason: &str) {
            // The close frame itself is plaintext, which is deliberate and worth stating: it
            // carries a code and a diagnostic sentence, never guest bytes, and a caller whose
            // handshake failed has no key to read an encrypted reason with.
            let frame = axum::extract::ws::CloseFrame {
                code,
                reason: reason.to_string().into(),
            };
            let _ = self.socket.send(Message::Close(Some(frame))).await;
        }

        fn chunk_bytes(&self) -> usize {
            NOISE_CHUNK_BYTES
        }
    }
}

/// Why a relay loop ended.
enum Ended {
    /// The caller sent a close frame or the stream ended.
    CallerClosed,
    /// The guest side reached EOF or refused a write.
    GuestClosed,
    /// A read on an established guest connection failed.
    RelayFailed,
    /// The WebSocket itself failed; no close frame can be delivered.
    TransportFailed,
}

/// Sends a close frame carrying `code` and `reason`, then drops the socket.
///
/// The reason string is for a human reading a log or an error message. It is deliberately
/// specific about *which* component refused — "nothing is listening in the guest" rather
/// than a bare failure — because the equivalent HTTP diagnostic (403 vs 502) is unavailable
/// on this path.
async fn close_with(mut socket: WebSocket, code: u16, reason: &str) {
    let frame = axum::extract::ws::CloseFrame {
        code,
        reason: reason.to_string().into(),
    };
    let _ = socket.send(Message::Close(Some(frame))).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_close_codes_are_in_the_private_range_and_distinct() {
        // 4000-4999 is the range RFC 6455 reserves for application use, so these cannot
        // collide with a platform close code — and every platform WebSocket failure is
        // 1006, so anything in this range is unambiguously ours.
        for code in [close::NO_LISTENER, close::BAD_PORT, close::RELAY_FAILED] {
            assert!(
                (4000..5000).contains(&code),
                "{code} is outside the private range"
            );
        }
        let mut seen = std::collections::BTreeSet::new();
        for code in [close::NO_LISTENER, close::BAD_PORT, close::RELAY_FAILED] {
            assert!(seen.insert(code), "{code} is used twice");
        }
    }

    #[test]
    fn the_dial_target_is_always_loopback() {
        // The module docs' egress claim, as an assertion: a relay that could be pointed at
        // another host would be an open proxy inside the VM reachable with the agent token.
        for port in [1_u16, 8080, 65535] {
            let target = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
            assert!(target.ip().is_loopback(), "{target} is not loopback");
            assert_eq!(target.port(), port);
        }
    }

    /// The chunk size is a **compile** error to get wrong, not a test failure.
    ///
    /// The repo's own remedy for a constant whose validity is a property of its value: a
    /// `const` block is checked when the crate builds, so a chunk size edited below an MTU
    /// cannot reach a test run at all. Clippy asks for this shape and it is the right one
    /// here — nothing about the assertion needs a runtime.
    const _: () = {
        assert!(
            RELAY_CHUNK_BYTES >= 1500,
            "a relay chunk below an MTU frames per packet"
        );
        assert!(
            RELAY_CHUNK_BYTES == 64 * 1024,
            "the chunk size is pinned: larger risks fragmentation we have not measured"
        );
    };
}
