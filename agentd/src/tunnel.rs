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
    let _ = &state;
    let port = query.port;
    upgrade
        .protocols([protocol::tunnel::WS_MARKER_SUBPROTOCOL])
        .on_upgrade(move |socket| relay(socket, port))
}

/// Dials the guest port and pumps bytes until either side ends.
async fn relay(mut socket: WebSocket, port: u16) {
    if port == 0 {
        close_with(
            socket,
            close::BAD_PORT,
            "port 0 cannot be dialled; name the guest port to relay to",
        )
        .await;
        return;
    }

    let target = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let stream = match tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect(target)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            tracing::info!(port, %error, "tunnel dial refused");
            close_with(
                socket,
                close::NO_LISTENER,
                &format!("nothing is listening on 127.0.0.1:{port} in the guest"),
            )
            .await;
            return;
        }
        Err(_) => {
            tracing::info!(port, "tunnel dial timed out");
            close_with(
                socket,
                close::NO_LISTENER,
                &format!("127.0.0.1:{port} did not accept within {DIAL_TIMEOUT:?}"),
            )
            .await;
            return;
        }
    };

    // `set_nodelay` because the payload is somebody else's protocol: Nagle would batch a
    // small request with whatever came next, and an interactive protocol (a database
    // handshake, a debugger step) would stall waiting for a peer that is waiting for it.
    if let Err(error) = stream.set_nodelay(true) {
        tracing::warn!(port, %error, "could not disable Nagle on the guest connection");
    }

    tracing::info!(port, "tunnel established");
    let (mut guest_read, mut guest_write) = stream.into_split();
    let mut buffer = vec![0_u8; RELAY_CHUNK_BYTES];

    // One task per direction would need a shared sink; a select loop keeps the WebSocket
    // owned here, which is what lets the close frame below be the last thing written.
    let outcome = loop {
        tokio::select! {
            inbound = socket.recv() => match inbound {
                // Binary is the contract. Text is accepted rather than refused because a
                // caller that sent it meant those bytes — refusing would be a second
                // failure mode for no gain — but nothing here produces text.
                Some(Ok(Message::Binary(bytes))) => {
                    if guest_write.write_all(&bytes).await.is_err() {
                        break Ended::GuestClosed;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    if guest_write.write_all(text.as_bytes()).await.is_err() {
                        break Ended::GuestClosed;
                    }
                }
                // A caller's half-close means "I have sent everything". Shutting down the
                // write half rather than dropping the whole connection is what lets a
                // request/response protocol get its response: `curl` and `psql` both send
                // then wait, and a full close here would truncate the answer.
                Some(Ok(Message::Close(_))) => break Ended::CallerClosed,
                Some(Ok(_)) => continue,
                Some(Err(error)) => {
                    tracing::info!(port, %error, "tunnel websocket error");
                    break Ended::TransportFailed;
                }
                None => break Ended::CallerClosed,
            },
            read = guest_read.read(&mut buffer) => match read {
                Ok(0) => break Ended::GuestClosed,
                Ok(count) => {
                    let frame = Message::Binary(buffer[..count].to_vec().into());
                    if socket.send(frame).await.is_err() {
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
            let _ = socket.send(Message::Close(None)).await;
        }
        Ended::RelayFailed => {
            close_with(
                socket,
                close::RELAY_FAILED,
                "the guest connection failed mid-relay",
            )
            .await;
        }
        // Nothing to send: the transport is what failed.
        Ended::TransportFailed => {}
    }
    tracing::info!(port, "tunnel closed");
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
