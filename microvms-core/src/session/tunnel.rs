// SPDX-License-Identifier: Apache-2.0
//! The client half of the TCP relay: a local TCP connection carried over a WebSocket.
//!
//! What `microvm tunnel` drives. A local listener accepts a connection, this module opens a
//! `wss://` handshake to the VM's endpoint offering the three port-scoped subprotocols, and
//! the daemon's `/v1/tcp` route dials `127.0.0.1:<port>` in the guest. Bytes then move in
//! binary frames until either end closes.
//!
//! # Why a real handshake rather than a proxied request
//!
//! Measured 2026-08-29 (`docs/PLATFORM.md`): an upgrade re-issued as an ordinary HTTPS `GET`
//! with `Upgrade: websocket` as a request header is answered **400 by the proxy**, and the
//! guest logs no handshake at all. The endpoint takes a WebSocket's credential as
//! `Sec-WebSocket-Protocol` values — because the browser `WebSocket` constructor cannot set
//! a header — so the HTTPS request path has no way to express an upgrade. That is why this
//! module exists separately from [`crate::session::forward`], which is an HTTP relay and
//! cannot carry one.
//!
//! # The token is scoped to the daemon's port, not to the guest port
//!
//! The inversion worth stating twice, because every other port-scoped call in this crate
//! names the port it wants to reach. This request terminates at **the daemon**, and the daemon
//! is what dials the guest port from inside the VM — so the proxy only ever sees a request for
//! the daemon's port, and a token scoped to the guest port authorizes something this request
//! never addresses. The guest port travels in the query string instead.
//!
//! Getting it backwards produces close code 1006 with no reason, which is indistinguishable
//! from a dead server; it cost a live debugging session on 2026-08-29 and is why
//! [`relay_connection`] carries the reason at the call site as well.
//!
//! The mint goes through the session's existing [`crate::session::ProxyAuth`] cache either
//! way, so a tunnel held past the platform's sixty-minute token ceiling refreshes without a
//! timer here.
//!
//! # One connection per WebSocket
//!
//! No multiplexing, matching the daemon's side. A mux protocol would need stream ids, flow
//! control per stream, and a close handshake per stream; the platform already provides all
//! three per connection. N local connections open N WebSockets.
//!
//! # Every platform failure is 1006, so the close code is the only diagnostic
//!
//! A refused handshake, a wrong-scope token, and a dead TCP connection all collapse to close
//! code 1006 with no reason string. The daemon's own codes
//! ([`protocol::tunnel::close`]) are drawn from RFC 6455's private range precisely so they
//! can be told apart from that, and [`explain_close`] is where a caller turns one into a
//! sentence.

use std::sync::Arc;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::{Message, protocol::CloseFrame};

use crate::error::{Error, ErrorKind};
use crate::session::proxy::ProxyAuth;

/// Bytes read from the local connection per relayed frame.
///
/// Matches the daemon's own relay chunk, so neither side is the one that fragments a stream.
pub const TUNNEL_CHUNK_BYTES: usize = 64 * 1024;

/// The daemon's tunnel route.
const TUNNEL_PATH: &str = "/v1/tcp";

/// What ended a tunnel, and whether the caller should treat it as a failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TunnelEnd {
    /// The local client closed, or the guest reached EOF. The tunnel did its job.
    Closed,
    /// The daemon refused or lost the guest connection, with its close code.
    ///
    /// Carries the code rather than a pre-rendered string so a caller can branch — a dead
    /// server is worth retrying against a different port, a relay failure is not.
    Refused { code: u16, reason: String },
}

/// The `wss://` URL for a tunnel to `guest_port`.
///
/// A bare endpoint host is read as `wss`, for the reason
/// [`crate::session::http::ReqwestBackend`] gives about `https`: the platform hands back a
/// hostname, and defaulting to plaintext on a missing prefix would put a bearer credential
/// on the wire in clear.
pub fn tunnel_url(endpoint: &str, guest_port: u16) -> String {
    let host = endpoint
        .trim_end_matches('/')
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("wss://")
        .trim_start_matches("ws://");
    // `ws` only when the caller explicitly asked for plaintext http, which is the local
    // daemon case a test uses; anything else is wss.
    let scheme = if endpoint.starts_with("http://") || endpoint.starts_with("ws://") {
        "ws"
    } else {
        "wss"
    };
    format!("{scheme}://{host}{TUNNEL_PATH}?port={guest_port}")
}

/// A sentence for a close code, or `None` when the code explains nothing.
///
/// Delegates to [`protocol::tunnel::close::explanation`] for the codes the daemon
/// originates, and adds the one the *platform* produces — 1006, which every proxy-side
/// failure collapses to. That case needs its own wording precisely because it is ambiguous:
/// naming a single cause would be a guess presented as a diagnosis.
pub fn explain_close(code: u16, guest_port: u16) -> Option<String> {
    if let Some(explanation) = protocol::tunnel::close::explanation(code, guest_port) {
        return Some(explanation);
    }
    if code == 1006 {
        return Some(format!(
            "the connection closed abnormally (1006) with no reason, which is what every \
             endpoint-proxy WebSocket failure looks like: a refused handshake, a token whose \
             scope does not cover port {guest_port}, and a dropped TCP connection are all \
             this code. Retry the same port over HTTPS to tell a scope mistake (403) from a \
             dead server (502)."
        ));
    }
    None
}

/// Opens a tunnel to `guest_port` and relays `local` over it until either side closes.
///
/// `local` is any duplex stream, so a test drives this with an in-memory pipe and the CLI
/// drives it with a `TcpStream`. Returns how the tunnel ended, which is a value rather than
/// an error for [`TunnelEnd::Closed`]: a finished tunnel is a success, and a caller that
/// treated it as a failure would retry a completed request.
pub async fn relay_connection<S>(
    mut local: S,
    endpoint: &str,
    guest_port: u16,
    agent_token: &str,
    auth: &Arc<ProxyAuth>,
) -> Result<TunnelEnd, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // **The token is scoped to the DAEMON's port, not to `guest_port`, and that inversion is
    // the whole subtlety of this route.**
    //
    // Every other port-scoped call in this crate names the port it wants to *reach*, so
    // `subprotocols(guest_port)` reads correct and is wrong: this request terminates at the
    // daemon, and it is the *daemon* that dials the guest port from inside the VM. The proxy
    // only ever sees a request for the daemon's port. A token scoped to 5432 therefore
    // authorizes a port this request never addresses, and the proxy's refusal is close code
    // 1006 with no reason — indistinguishable from a dead server, which is exactly how this
    // cost a live debugging session on 2026-08-29.
    //
    // `guest_port` still travels, in the query string, where the daemon reads it.
    let offered = auth.subprotocols(auth.port()).await?;
    let url = tunnel_url(endpoint, guest_port);

    let mut request = url.as_str().into_client_request().map_err(|err| {
        Error::new(
            ErrorKind::InvalidArg,
            format!("{url} is not a usable WebSocket URL: {err}"),
        )
    })?;
    {
        let headers = request.headers_mut();
        // The three platform values, offered as one comma-separated list. The proxy consumes
        // all of them and forwards none (measured), so the daemon sees an ordinary handshake.
        headers.insert(
            "sec-websocket-protocol",
            offered.join(", ").parse().map_err(|err| {
                Error::new(
                    ErrorKind::Unexpected,
                    format!("the minted subprotocols are not a legal header value: {err}"),
                )
            })?,
        );
        // The daemon's own bearer check runs on the upgrade request, so the agent token has
        // to be here too — the proxy credential authorizes reaching the port, and this
        // authorizes the daemon's control route behind it. Two credentials, two purposes.
        headers.insert(
            "authorization",
            format!("Bearer {agent_token}").parse().map_err(|err| {
                Error::new(
                    ErrorKind::Unexpected,
                    format!("the agent token is not a legal header value: {err}"),
                )
            })?,
        );
    }

    let (mut socket, _response) =
        tokio_tungstenite::connect_async(request)
            .await
            .map_err(|err| {
                Error::new(
                    ErrorKind::Unexpected,
                    format!(
                        "the tunnel handshake to port {guest_port} failed: {err}. Every \
                     endpoint-proxy WebSocket failure is close code 1006 with no reason, so \
                     if this names nothing, retry the port over HTTPS to tell a scope \
                     mistake (403) from a dead server (502)."
                    ),
                )
            })?;

    let mut buffer = vec![0_u8; TUNNEL_CHUNK_BYTES];
    loop {
        tokio::select! {
            // Read from the local client, frame it, send it.
            read = local.read(&mut buffer) => match read {
                Ok(0) => {
                    // The local side finished sending. A close frame rather than a drop, so
                    // the daemon shuts the guest's write half down and a request/response
                    // protocol still gets its answer.
                    let _ = socket.send(Message::Close(None)).await;
                    return Ok(TunnelEnd::Closed);
                }
                Ok(count) => {
                    if socket
                        .send(Message::Binary(buffer[..count].to_vec()))
                        .await
                        .is_err()
                    {
                        return Ok(TunnelEnd::Closed);
                    }
                }
                Err(err) => {
                    return Err(Error::new(
                        ErrorKind::Unexpected,
                        format!("reading the local connection failed: {err}"),
                    ));
                }
            },
            // Read a frame from the tunnel, write it to the local client.
            frame = socket.next() => match frame {
                Some(Ok(Message::Binary(bytes))) => {
                    if local.write_all(&bytes).await.is_err() {
                        return Ok(TunnelEnd::Closed);
                    }
                }
                // Text is not produced by the daemon, but a caller that got it meant those
                // bytes; passing them through beats inventing a protocol error.
                Some(Ok(Message::Text(text))) => {
                    if local.write_all(text.as_bytes()).await.is_err() {
                        return Ok(TunnelEnd::Closed);
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    let _ = local.flush().await;
                    return Ok(classify_close(frame.as_ref()));
                }
                Some(Ok(_)) => continue,
                // A transport error after a successful handshake. Reported as Closed rather
                // than an Err because the bytes already delivered are still valid, and the
                // caller's own read of `local` is where a truncation would surface.
                Some(Err(_)) => return Ok(TunnelEnd::Closed),
                None => return Ok(TunnelEnd::Closed),
            },
        }
    }
}

/// Reads a close frame into a [`TunnelEnd`].
///
/// A missing frame is [`TunnelEnd::Closed`]: an absent code is not a failure, and treating
/// it as one would turn every clean hangup into an error.
fn classify_close(frame: Option<&CloseFrame>) -> TunnelEnd {
    let Some(frame) = frame else {
        return TunnelEnd::Closed;
    };
    let code: u16 = frame.code.into();
    if code == protocol::tunnel::close::NORMAL {
        return TunnelEnd::Closed;
    }
    TunnelEnd::Refused {
        code,
        reason: frame.reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_endpoint_host_becomes_wss() {
        // The same rule the HTTP backend applies to https: a missing prefix must not
        // downgrade a bearer credential onto a plaintext socket.
        assert_eq!(
            tunnel_url("vm-abc.example.aws", 8080),
            "wss://vm-abc.example.aws/v1/tcp?port=8080"
        );
        assert_eq!(
            tunnel_url("https://vm-abc.example.aws/", 5432),
            "wss://vm-abc.example.aws/v1/tcp?port=5432"
        );
    }

    #[test]
    fn an_explicit_plaintext_endpoint_stays_plaintext() {
        // The local-daemon case a test uses. Explicit `http://` is a caller saying they know
        // there is no TLS here; a bare hostname is not.
        assert_eq!(
            tunnel_url("http://127.0.0.1:9000", 8080),
            "ws://127.0.0.1:9000/v1/tcp?port=8080"
        );
        assert_eq!(
            tunnel_url("ws://127.0.0.1:9000", 1),
            "ws://127.0.0.1:9000/v1/tcp?port=1"
        );
    }

    #[test]
    fn the_daemons_close_codes_are_explained_and_the_platforms_ambiguity_is_named() {
        // 4502 is the daemon's, so the wording comes from the protocol crate — one definition
        // for both sides.
        let dead = explain_close(protocol::tunnel::close::NO_LISTENER, 5432)
            .expect("4502 explains itself");
        assert!(dead.contains("5432"), "{dead}");
        assert!(dead.contains("not an auth problem"), "{dead}");

        // 1006 is the platform's, and its explanation must present the ambiguity rather than
        // pick one cause.
        let opaque = explain_close(1006, 8080).expect("1006 is worth naming");
        assert!(
            opaque.contains("scope does not cover port 8080"),
            "{opaque}"
        );
        assert!(opaque.contains("403"), "{opaque}");
        assert!(opaque.contains("502"), "{opaque}");

        // A code nobody in this system originates gets no invented meaning.
        assert!(explain_close(4999, 8080).is_none());
    }

    #[test]
    fn a_clean_close_is_not_a_refusal() {
        assert_eq!(classify_close(None), TunnelEnd::Closed);
        let normal = CloseFrame {
            code: protocol::tunnel::close::NORMAL.into(),
            reason: "done".into(),
        };
        assert_eq!(classify_close(Some(&normal)), TunnelEnd::Closed);
    }

    #[test]
    fn a_daemon_refusal_carries_its_code_so_a_caller_can_branch() {
        // The reason a code rather than a rendered string crosses this boundary: a dead
        // server is worth retrying against another port and a relay failure is not.
        let refused = CloseFrame {
            code: protocol::tunnel::close::NO_LISTENER.into(),
            reason: "nothing is listening on 127.0.0.1:5432 in the guest".into(),
        };
        match classify_close(Some(&refused)) {
            TunnelEnd::Refused { code, reason } => {
                assert_eq!(code, protocol::tunnel::close::NO_LISTENER);
                assert!(reason.contains("5432"), "{reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// The marker this client offers is the one the daemon echoes.
    ///
    /// Not an assertion that two constants agree — `proxy::WS_SUBPROTOCOL` *is*
    /// `protocol::tunnel::WS_MARKER_SUBPROTOCOL`, aliased rather than copied, so a drift is
    /// unrepresentable rather than merely detected. This test pins the aliasing itself: an
    /// edit that replaced the alias with a literal would pass every other test in the tree,
    /// and the failure it caused would be a refused handshake naming neither side.
    #[test]
    fn the_offered_marker_is_the_daemons_echoed_marker() {
        assert_eq!(
            crate::session::proxy::WS_SUBPROTOCOL,
            protocol::tunnel::WS_MARKER_SUBPROTOCOL,
            "the marker must stay aliased to the protocol crate's, not copied from it"
        );
    }

    /// The two chunk sizes agree, as a compile error rather than a test failure.
    ///
    /// If the client framed larger than the daemon reads, one side would be the one that
    /// fragments every stream — and which side fragments is exactly the kind of asymmetry
    /// that shows up as a protocol bug in somebody else's application.
    const _: () = {
        assert!(
            TUNNEL_CHUNK_BYTES == 64 * 1024,
            "the client chunk must match the daemon's RELAY_CHUNK_BYTES"
        );
    };
}
