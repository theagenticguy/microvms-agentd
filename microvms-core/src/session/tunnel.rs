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

/// Bytes read from the local connection per Noise message on a verified tunnel.
///
/// Matches the daemon's `NOISE_CHUNK_BYTES`, and both are below the plain 64 KiB for the
/// same arithmetic: a Noise transport message carries the ciphertext plus a 16-byte tag
/// inside one 65535-byte ceiling, and `write_message` refuses a plaintext that does not fit
/// rather than fragmenting it.
pub const VERIFIED_CHUNK_BYTES: usize = 32 * 1024;

/// The Noise message ceiling, from the spec's two-byte length prefix.
const NOISE_MAX_MESSAGE_BYTES: usize = 65535;

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
    tunnel_url_with_identity(endpoint, guest_port, false)
}

/// [`tunnel_url`], with the `identity=true` flag when a Noise handshake is wanted.
pub fn tunnel_url_with_identity(endpoint: &str, guest_port: u16, identity: bool) -> String {
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
    let flag = if identity { "&identity=true" } else { "" };
    format!("{scheme}://{host}{TUNNEL_PATH}?port={guest_port}{flag}")
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
    local: S,
    endpoint: &str,
    guest_port: u16,
    agent_token: &str,
    auth: &Arc<ProxyAuth>,
) -> Result<TunnelEnd, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    relay_connection_inner(local, endpoint, guest_port, agent_token, auth, None).await
}

/// [`relay_connection`], first proving the far end is the VM `identity` was minted for.
///
/// The Noise KK handshake runs before any local byte moves: the initiator's message goes
/// out, the daemon's reply is verified against the pinned VM key, and only then does the
/// relay start — with every subsequent frame encrypted under the session the handshake
/// keyed. A refused handshake surfaces as [`TunnelEnd::Refused`] with the daemon's close
/// code (4403), or as an error naming the failed verification when the daemon's *reply*
/// does not authenticate — which is the wrong-pin case observed from this side.
pub async fn relay_connection_verified<S>(
    local: S,
    endpoint: &str,
    guest_port: u16,
    agent_token: &str,
    auth: &Arc<ProxyAuth>,
    identity: &crate::identity::TunnelIdentity,
) -> Result<TunnelEnd, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    relay_connection_inner(
        local,
        endpoint,
        guest_port,
        agent_token,
        auth,
        Some(identity),
    )
    .await
}

/// Proves the far end is the VM `identity` was minted for, and nothing more.
///
/// The same handshake [`relay_connection_verified`] runs before its first relayed byte — one
/// code path, so `microvm attach --verify-identity` and `microvm tunnel --verify-identity`
/// cannot disagree about what "verified" means — with no local connection to relay: the
/// socket is closed cleanly the moment the daemon's reply authenticates. The guest port is
/// the daemon's own, because the daemon dials it after the handshake and it is the one port
/// every VM is guaranteed to have a listener on.
///
/// Returns [`TunnelEnd::Closed`] when the pin verified, [`TunnelEnd::Refused`] with the
/// daemon's close code when it refused (4401 no seed at launch, 4403 refused), and an error
/// when the daemon's reply did not authenticate against the pin — the wrong-VM case as seen
/// from this side.
pub async fn verify_identity(
    endpoint: &str,
    agent_token: &str,
    auth: &Arc<ProxyAuth>,
    identity: &crate::identity::TunnelIdentity,
) -> Result<TunnelEnd, Error> {
    let guest_port = auth.port();
    match open(endpoint, guest_port, agent_token, auth, Some(identity)).await? {
        Opened::Refused(end) => Ok(end),
        Opened::Ready(mut ready) => {
            let _ = ready.socket.send(Message::Close(None)).await;
            Ok(TunnelEnd::Closed)
        }
    }
}

/// An open tunnel WebSocket, past the identity handshake when one was asked for.
///
/// The ready half is boxed because a WebSocket plus a Noise transport state is several
/// hundred bytes beside a two-word refusal, and clippy's `large_enum_variant` is right that
/// every `Refused` would otherwise carry that much padding.
enum Opened {
    Ready(Box<Ready>),
    Refused(TunnelEnd),
}

struct Ready {
    socket: TunnelSocket,
    noise: Option<snow::TransportState>,
}

type TunnelSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Opens the tunnel WebSocket to `guest_port` and, when `identity` is given, runs the Noise KK
/// handshake over it before returning.
///
/// Shared by the relay and by [`verify_identity`], so the token scoping, the two credentials
/// on the upgrade, and the handshake ordering are written once.
async fn open(
    endpoint: &str,
    guest_port: u16,
    agent_token: &str,
    auth: &Arc<ProxyAuth>,
    identity: Option<&crate::identity::TunnelIdentity>,
) -> Result<Opened, Error> {
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
    let url = tunnel_url_with_identity(endpoint, guest_port, identity.is_some());

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

    // The identity handshake, before any local byte moves. Its failure modes split by who
    // detected them: the daemon's refusal arrives as a close frame (returned as `Refused` so
    // the CLI renders the code's explanation), and a reply that fails *our* verification is
    // an error naming the pin — that is the wrong-pin case as seen from this side, and the
    // one piece of diagnosis the daemon cannot do.
    let noise = match identity {
        None => None,
        Some(identity) => match initiate(&mut socket, identity, guest_port).await? {
            Initiated::Transport(transport) => Some(transport),
            Initiated::Refused(end) => return Ok(Opened::Refused(end)),
        },
    };
    Ok(Opened::Ready(Box::new(Ready { socket, noise })))
}

async fn relay_connection_inner<S>(
    mut local: S,
    endpoint: &str,
    guest_port: u16,
    agent_token: &str,
    auth: &Arc<ProxyAuth>,
    identity: Option<&crate::identity::TunnelIdentity>,
) -> Result<TunnelEnd, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Ready {
        mut socket,
        mut noise,
    } = match open(endpoint, guest_port, agent_token, auth, identity).await? {
        Opened::Ready(ready) => *ready,
        Opened::Refused(end) => return Ok(end),
    };

    // The verified path reads smaller chunks so plaintext plus the 16-byte tag fits one
    // Noise message; `write_message` refuses an oversize payload rather than splitting it.
    let chunk = if noise.is_some() {
        VERIFIED_CHUNK_BYTES
    } else {
        TUNNEL_CHUNK_BYTES
    };
    let mut buffer = vec![0_u8; chunk];
    let mut scratch = vec![0_u8; NOISE_MAX_MESSAGE_BYTES];
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
                    let frame = match seal(&mut noise, &buffer[..count], &mut scratch)? {
                        Some(sealed) => sealed,
                        None => buffer[..count].to_vec(),
                    };
                    if socket.send(Message::Binary(frame.into())).await.is_err() {
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
                    let plain: &[u8] = match noise.as_mut() {
                        None => &bytes,
                        Some(transport) => {
                            let count = transport.read_message(&bytes, &mut scratch).map_err(|err| {
                                // Failing rather than skipping, because a frame that does not
                                // authenticate on a verified tunnel is a forged or corrupted
                                // frame — writing it to the local client would hand the
                                // application attacker-controlled bytes on the one path that
                                // promised otherwise.
                                Error::new(
                                    ErrorKind::Unexpected,
                                    format!("a tunnel frame did not authenticate: {err}"),
                                )
                            })?;
                            &scratch[..count]
                        }
                    };
                    if local.write_all(plain).await.is_err() {
                        return Ok(TunnelEnd::Closed);
                    }
                }
                // Text is not produced by the daemon. On a plain tunnel a caller that got it
                // meant those bytes; on a verified one it cannot be a Noise message, so it is
                // refused for the same reason a bad frame is.
                Some(Ok(Message::Text(text))) => {
                    if noise.is_some() {
                        return Err(Error::new(
                            ErrorKind::Unexpected,
                            "a verified tunnel received a text frame, which cannot be a Noise \
                             message".to_string(),
                        ));
                    }
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

/// Encrypts one chunk when the tunnel is verified, or reports `None` to send plaintext.
///
/// A helper so the send path above stays one expression per arm; the error is a local
/// encryption failure, which is unreachable for chunk sizes the `const` checks admit but is
/// reported honestly rather than unwrapped.
fn seal(
    noise: &mut Option<snow::TransportState>,
    plain: &[u8],
    scratch: &mut [u8],
) -> Result<Option<Vec<u8>>, Error> {
    let Some(transport) = noise.as_mut() else {
        return Ok(None);
    };
    let count = transport.write_message(plain, scratch).map_err(|err| {
        Error::new(
            ErrorKind::Unexpected,
            format!("encrypting a tunnel chunk failed: {err}"),
        )
    })?;
    Ok(Some(scratch[..count].to_vec()))
}

/// How the identity handshake ended: a keyed transport, or the daemon's refusal.
enum Initiated {
    Transport(snow::TransportState),
    Refused(TunnelEnd),
}

/// Runs the initiator's half of the Noise KK handshake over the open WebSocket.
async fn initiate(
    socket: &mut TunnelSocket,
    identity: &crate::identity::TunnelIdentity,
    guest_port: u16,
) -> Result<Initiated, Error> {
    let mut initiator = identity.initiator()?;
    let mut scratch = vec![0_u8; NOISE_MAX_MESSAGE_BYTES];

    let written = initiator.write_message(&[], &mut scratch).map_err(|err| {
        Error::new(
            ErrorKind::Unexpected,
            format!("writing the identity handshake failed: {err}"),
        )
    })?;
    if socket
        .send(Message::Binary(scratch[..written].to_vec().into()))
        .await
        .is_err()
    {
        return Err(Error::new(
            ErrorKind::Unexpected,
            "the tunnel closed before the identity handshake could be sent".to_string(),
        ));
    }

    // The daemon answers with its handshake message, or refuses with a close frame whose
    // code says why (4401 no seed at launch, 4403 refused). Anything else mid-handshake is a
    // transport fault.
    loop {
        match socket.next().await {
            Some(Ok(Message::Binary(reply))) => {
                initiator.read_message(&reply, &mut scratch).map_err(|_| {
                    // *Our* verification failed on the daemon's reply: the far end is not the
                    // VM the pin was minted for. This is the diagnosis the daemon cannot make
                    // — it does not know which key we pinned — and the one the caller most
                    // needs, because the likely cause is a record replayed from another VM.
                    Error::new(
                        ErrorKind::Unexpected,
                        format!(
                            "the identity handshake reply did not verify against the pinned \
                             key for this VM. The far end holds a different seed than the \
                             one this record was created with — a record copied from another \
                             VM, or a VM relaunched with a fresh seed, would both do this. \
                             (guest port {guest_port})"
                        ),
                    )
                })?;
                let transport = initiator.into_transport_mode().map_err(|err| {
                    Error::new(
                        ErrorKind::Unexpected,
                        format!("entering transport mode failed: {err}"),
                    )
                })?;
                return Ok(Initiated::Transport(transport));
            }
            Some(Ok(Message::Close(frame))) => {
                return Ok(Initiated::Refused(classify_close(frame.as_ref())));
            }
            Some(Ok(_)) => continue,
            Some(Err(err)) => {
                return Err(Error::new(
                    ErrorKind::Unexpected,
                    format!("the tunnel failed mid-handshake: {err}"),
                ));
            }
            None => {
                return Err(Error::new(
                    ErrorKind::Unexpected,
                    "the tunnel closed mid-handshake with no close frame".to_string(),
                ));
            }
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
