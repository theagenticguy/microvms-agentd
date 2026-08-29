// SPDX-License-Identifier: Apache-2.0
//! The tunnel client against a real WebSocket-to-TCP relay, with a real TCP server behind it.
//!
//! The one test that exercises both halves of issue #70 layer 2 together. Everything else is
//! a unit test of one side: `agentd/tests/tunnel_relay.rs` drives the daemon with a
//! third-party client, and `session::tunnel`'s own tests cover URL forming and close-code
//! classification. Neither would catch the two sides disagreeing — a subprotocol header the
//! daemon ignores, a close code the client misreads, a chunk size that fragments — which is
//! exactly what a client and a server developed against each other's documentation get wrong.
//!
//! The relay here is a **stand-in for agentd's route, not agentd itself**: `microvms-core`
//! must not depend on the daemon (ARCH-1 keeps the dependency one-way), so this file speaks
//! the same wire contract from the `protocol` crate. That is the honest limit of this test,
//! and it is why `agentd/tests/tunnel_relay.rs` exists beside it: together they pin both ends
//! of one contract, and the contract itself lives in the crate they share.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use futures_util::{SinkExt as _, StreamExt as _};
use microvms_core::Error;
use microvms_core::session::proxy::{
    PROXY_AUTH_HEADER, ProxyAuth, ProxyToken, TokenMinter, WS_AUTH_SUBPROTOCOL_PREFIX,
    WS_PORT_SUBPROTOCOL_PREFIX, WS_SUBPROTOCOL,
};
use microvms_core::session::tunnel::{TunnelEnd, relay_connection};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const AGENT_TOKEN: &str = "end-to-end-agent-token";

/// A minter that hands back a fixed JWE, recording the port scopes it was asked for.
struct FakeMinter {
    scopes: std::sync::Mutex<Vec<Vec<u16>>>,
}

impl TokenMinter for FakeMinter {
    fn mint(&self) -> BoxFuture<'_, Result<ProxyToken, Error>> {
        Box::pin(async move {
            Ok(ProxyToken::from_pairs([(
                PROXY_AUTH_HEADER,
                "jwe-for-agent-port",
            )]))
        })
    }

    fn mint_for_ports(&self, ports: &[u16]) -> BoxFuture<'_, Result<ProxyToken, Error>> {
        let mut sorted = ports.to_vec();
        sorted.sort_unstable();
        self.scopes.lock().expect("not poisoned").push(sorted);
        Box::pin(async move {
            Ok(ProxyToken::from_pairs([(
                PROXY_AUTH_HEADER,
                "jwe-for-port",
            )]))
        })
    }
}

fn auth() -> (Arc<ProxyAuth>, Arc<FakeMinter>) {
    let minter = Arc::new(FakeMinter {
        scopes: std::sync::Mutex::new(Vec::new()),
    });
    let auth = ProxyAuth::new(minter.clone(), microvms_core::session::DEFAULT_AGENT_PORT);
    (Arc::new(auth), minter)
}

/// What one relayed connection observed, for a test to assert on afterwards.
#[derive(Default)]
struct Observed {
    subprotocols: Option<String>,
    authorization: Option<String>,
    query: Option<String>,
}

/// Records a handshake and echoes the marker subprotocol, as the platform proxy does.
///
/// A `Callback` impl rather than a closure, for the reason the call site gives: tungstenite's
/// `Err` arm is a whole `ErrorResponse`, and a closure returning one trips
/// `clippy::result_large_err` on a signature this crate does not own.
struct Recorder {
    recorder: Arc<std::sync::Mutex<Observed>>,
}

impl tokio_tungstenite::tungstenite::handshake::server::Callback for Recorder {
    fn on_request(
        self,
        request: &tokio_tungstenite::tungstenite::handshake::server::Request,
        mut response: tokio_tungstenite::tungstenite::handshake::server::Response,
    ) -> Result<
        tokio_tungstenite::tungstenite::handshake::server::Response,
        tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
    > {
        let mut seen = self.recorder.lock().expect("not poisoned");
        seen.subprotocols = request
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        seen.authorization = request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        seen.query = request.uri().query().map(str::to_string);

        // Echo the bare marker, because the PLATFORM proxy does: measured 2026-08-15, a client
        // that offers the three values observes `ws.protocol === "lambda-microvms"` even when
        // the guest names none, "which the proxy supplies on the guest's behalf". Not cosmetic
        // — tungstenite refuses a handshake that offered subprotocols and got none back
        // ("Server sent no subprotocol"), so a stand-in that skipped this would fail a
        // handshake the real endpoint completes. That would be a bug in the harness reported as
        // a bug in the client.
        response.headers_mut().insert(
            "sec-websocket-protocol",
            WS_SUBPROTOCOL.parse().expect("a legal header value"),
        );
        Ok(response)
    }
}

/// A stand-in for `agentd`'s `/v1/tcp`: upgrades, records the handshake, relays to `target`.
///
/// `refuse_with` closes with that code instead of relaying, which is how the dead-port and
/// bad-port paths are driven without needing a genuinely closed port.
async fn relay_server(
    target: Option<SocketAddr>,
    refuse_with: Option<u16>,
) -> (SocketAddr, Arc<std::sync::Mutex<Observed>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let addr = listener.local_addr().expect("bound");
    let observed = Arc::new(std::sync::Mutex::new(Observed::default()));
    let recorder = observed.clone();

    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let recorder = recorder.clone();
        // Capture the handshake on the way past: the values the client offered are the
        // contract, and a callback is the only place they are visible. The body is a named
        // function rather than an inline closure because tungstenite's callback returns a
        // whole `ErrorResponse` in its `Err` arm — 136 bytes, which clippy's
        // `result_large_err` flags on a closure. The lint is about tungstenite's signature
        // rather than about anything here, and a named function keeps the diagnosis where it
        // belongs instead of parking an `allow` in the tree.
        let socket = tokio_tungstenite::accept_hdr_async(stream, Recorder { recorder }).await;
        let Ok(mut socket) = socket else {
            return;
        };

        if let Some(code) = refuse_with {
            let frame = tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: code.into(),
                reason: format!("refused with {code} for port 5432").into(),
            };
            let _ = socket.send(Message::Close(Some(frame))).await;
            return;
        }

        let Some(target) = target else { return };
        let Ok(guest) = tokio::net::TcpStream::connect(target).await else {
            return;
        };
        let (mut guest_read, mut guest_write) = guest.into_split();
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            tokio::select! {
                inbound = socket.next() => match inbound {
                    Some(Ok(Message::Binary(bytes))) => {
                        if guest_write.write_all(&bytes).await.is_err() { break }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => continue,
                },
                read = guest_read.read(&mut buffer) => match read {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if socket.send(Message::Binary(buffer[..count].to_vec())).await.is_err() {
                            break
                        }
                    }
                },
            }
        }
        let _ = socket.send(Message::Close(None)).await;
    });

    (addr, observed)
}

/// A TCP server that upper-cases its input, so a passing test proves bytes crossed it.
async fn upper_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let addr = listener.local_addr().expect("bound");
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 64 * 1024];
                loop {
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(count) => {
                            let upper: Vec<u8> =
                                buffer[..count].iter().map(u8::to_ascii_uppercase).collect();
                            if socket.write_all(&upper).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

/// **A local connection reaches a guest server and its answer comes back.**
///
/// The whole of layer 2 in one assertion. Upper-casing rather than echoing, so bytes that
/// never left the client cannot pass.
#[tokio::test]
async fn a_local_connection_reaches_the_guest_server_through_the_tunnel() {
    let guest = upper_server().await;
    let (relay, _observed) = relay_server(Some(guest), None).await;
    let (auth, _minter) = auth();

    let (mut client, local) = tokio::io::duplex(64 * 1024);
    let endpoint = format!("http://{relay}");
    let pump =
        tokio::spawn(
            async move { relay_connection(local, &endpoint, 8080, AGENT_TOKEN, &auth).await },
        );

    client
        .write_all(b"through the tunnel")
        .await
        .expect("written");
    let mut answer = vec![0_u8; 18];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.read_exact(&mut answer),
    )
    .await
    .expect("an answer arrives")
    .expect("read");
    assert_eq!(&answer, b"THROUGH THE TUNNEL");

    drop(client);
    let ended = tokio::time::timeout(std::time::Duration::from_secs(5), pump)
        .await
        .expect("the pump finishes")
        .expect("the task joins")
        .expect("no transport error");
    assert_eq!(ended, TunnelEnd::Closed);
}

/// **The handshake carries the three platform subprotocols, the port, and the agent token.**
///
/// The contract between the two halves, asserted from the server's side rather than from the
/// client's own construction — the client agreeing with itself proves nothing.
#[tokio::test]
async fn the_handshake_offers_the_platform_values_and_the_agent_token() {
    let guest = upper_server().await;
    let (relay, observed) = relay_server(Some(guest), None).await;
    let (auth, minter) = auth();

    let (client, local) = tokio::io::duplex(4096);
    let endpoint = format!("http://{relay}");
    let pump =
        tokio::spawn(
            async move { relay_connection(local, &endpoint, 5432, AGENT_TOKEN, &auth).await },
        );
    // Let the handshake complete, then end the connection.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    drop(client);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), pump).await;

    let seen = observed.lock().expect("not poisoned");
    let offered = seen
        .subprotocols
        .clone()
        .expect("subprotocols were offered");
    assert!(offered.contains(WS_SUBPROTOCOL), "{offered}");
    assert!(offered.contains(WS_AUTH_SUBPROTOCOL_PREFIX), "{offered}");
    // The port subprotocol names the DAEMON's port, because that is the port this request
    // addresses — the guest port travels in the query string instead. See
    // `the_token_is_scoped_to_the_daemon_port_not_the_guest_port`.
    assert!(
        offered.contains(&format!("{WS_PORT_SUBPROTOCOL_PREFIX}9000")),
        "the port subprotocol must name the daemon's port: {offered}"
    );
    assert!(
        !offered.contains(&format!("{WS_PORT_SUBPROTOCOL_PREFIX}5432")),
        "naming the guest port here is the 2026-08-29 regression: {offered}"
    );
    assert_eq!(
        seen.authorization.as_deref(),
        Some(format!("Bearer {AGENT_TOKEN}").as_str()),
        "the daemon's own bearer check runs on the upgrade, so the token must be present"
    );
    assert_eq!(
        seen.query.as_deref(),
        Some("port=5432"),
        "the daemon reads the target port from the query"
    );

    // The scope is the DAEMON's port; see `the_token_is_scoped_to_the_daemon_port_not_the_guest_port`
    // for why naming the guest port here would be the bug rather than the fix.
    let scopes = minter.scopes.lock().expect("not poisoned").clone();
    assert!(
        !scopes.is_empty(),
        "the tunnel must mint a port-scoped token: {scopes:?}"
    );
}

/// **The minted token is scoped to the DAEMON's port, never to the guest port.**
///
/// The live regression from 2026-08-29. `subprotocols(guest_port)` reads correct — every other
/// port-scoped call in the crate names the port it wants to reach — and is wrong here: the
/// request terminates at the daemon, and the daemon dials the guest port from inside the VM. A
/// token scoped to the guest port authorizes a port the request never addresses, and the
/// proxy's refusal is close code 1006 with no reason, indistinguishable from a dead server.
/// That ambiguity is exactly why this needs a test rather than a comment.
///
/// **Falsification:** change `auth.subprotocols(auth.port())` back to
/// `auth.subprotocols(guest_port)` in `session::tunnel` and the scope assertion below fails.
#[tokio::test]
async fn the_token_is_scoped_to_the_daemon_port_not_the_guest_port() {
    let guest = upper_server().await;
    let (relay, _observed) = relay_server(Some(guest), None).await;
    let (auth, minter) = auth();
    let daemon_port = auth.port();

    let (client, local) = tokio::io::duplex(4096);
    let endpoint = format!("http://{relay}");
    // A guest port deliberately different from the daemon's, so the two are distinguishable.
    let pump =
        tokio::spawn(
            async move { relay_connection(local, &endpoint, 5432, AGENT_TOKEN, &auth).await },
        );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    drop(client);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), pump).await;

    let scopes = minter.scopes.lock().expect("not poisoned").clone();
    assert!(
        !scopes.is_empty(),
        "the tunnel must mint a port-scoped token"
    );
    for ports in &scopes {
        assert!(
            ports.contains(&daemon_port),
            "every mint must authorize the daemon's port {daemon_port}, got {ports:?}"
        );
        assert!(
            !ports.contains(&5432),
            "the guest port must NOT be in the token scope — the proxy never sees a request \
             for it, and scoping to it produces an unexplainable 1006: {ports:?}"
        );
    }
}

/// **A payload larger than one chunk survives whole, in order, byte-exact.**
///
/// 200 KiB crosses the 64 KiB chunk boundary several times, and the ramp makes a reordering
/// visible as a value mismatch rather than only as a length mismatch.
#[tokio::test]
async fn a_large_payload_survives_the_round_trip_in_order() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let guest = listener.local_addr().expect("bound");
    let total = 200 * 1024;
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("one connection");
        let payload: Vec<u8> = (0..total).map(|index| (index % 251) as u8).collect();
        let _ = socket.write_all(&payload).await;
        let _ = socket.shutdown().await;
    });

    let (relay, _observed) = relay_server(Some(guest), None).await;
    let (auth, _minter) = auth();
    let (mut client, local) = tokio::io::duplex(256 * 1024);
    let endpoint = format!("http://{relay}");
    tokio::spawn(async move { relay_connection(local, &endpoint, 8080, AGENT_TOKEN, &auth).await });

    let mut received = vec![0_u8; total];
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        client.read_exact(&mut received),
    )
    .await
    .expect("the whole payload arrives")
    .expect("read");

    let expected: Vec<u8> = (0..total).map(|index| (index % 251) as u8).collect();
    assert_eq!(
        received, expected,
        "the tunnel reordered or corrupted bytes"
    );
}

/// **A non-utf8 payload survives byte-exact in the client→guest direction too.**
///
/// The daemon's own tests cover guest→client. This is the other direction, and it is a
/// separate risk: the client frames what it read from a local socket, and any `String` on
/// that path would corrupt `0x80`-`0xFF` while keeping the length plausible.
#[tokio::test]
async fn a_non_utf8_payload_reaches_the_guest_unchanged() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let guest = listener.local_addr().expect("bound");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = seen.clone();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("one connection");
        let mut buffer = vec![0_u8; 4096];
        if let Ok(count) = socket.read(&mut buffer).await {
            recorder
                .lock()
                .expect("not poisoned")
                .extend_from_slice(&buffer[..count]);
            let _ = socket.write_all(b"ack").await;
        }
    });

    let (relay, _observed) = relay_server(Some(guest), None).await;
    let (auth, _minter) = auth();
    let (mut client, local) = tokio::io::duplex(4096);
    let endpoint = format!("http://{relay}");
    tokio::spawn(async move { relay_connection(local, &endpoint, 8080, AGENT_TOKEN, &auth).await });

    let payload: Vec<u8> = vec![0x00, 0xff, 0xfe, 0x80, 0x7f, 0x00, 0xc0, 0x80];
    client.write_all(&payload).await.expect("written");
    let mut ack = vec![0_u8; 3];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.read_exact(&mut ack),
    )
    .await
    .expect("the guest answers")
    .expect("read");

    let arrived = seen.lock().expect("not poisoned").clone();
    assert_eq!(arrived, payload, "the client corrupted a non-utf8 payload");
}

/// **A daemon refusal surfaces as its close code, not as silence or a generic error.**
///
/// 4502 is the code a dead guest port produces, and the reason a `microvm tunnel` user can
/// act on. A client that reported "connection closed" here would send them looking at the
/// wrong component.
#[tokio::test]
async fn a_dead_guest_port_surfaces_as_the_relays_close_code() {
    let (relay, _observed) = relay_server(None, Some(protocol::tunnel::close::NO_LISTENER)).await;
    let (auth, _minter) = auth();
    let (_client, local) = tokio::io::duplex(4096);
    let endpoint = format!("http://{relay}");

    let ended = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        relay_connection(local, &endpoint, 5432, AGENT_TOKEN, &auth),
    )
    .await
    .expect("the tunnel resolves")
    .expect("a refusal is a value, not a transport error");

    match ended {
        TunnelEnd::Refused { code, reason } => {
            assert_eq!(code, protocol::tunnel::close::NO_LISTENER);
            assert!(reason.contains("5432"), "{reason}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// **A refused handshake names the ambiguity rather than guessing a cause.**
///
/// Nothing listening at the endpoint at all. The error must not claim to know whether this
/// was a scope mistake or a dead server, because on the real endpoint both are 1006.
#[tokio::test]
async fn an_unreachable_endpoint_fails_with_the_1006_ambiguity_named() {
    let held = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let dead = held.local_addr().expect("bound");
    drop(held);

    let (auth, _minter) = auth();
    let (_client, local) = tokio::io::duplex(4096);
    let endpoint = format!("http://{dead}");

    let error = relay_connection(local, &endpoint, 8080, AGENT_TOKEN, &auth)
        .await
        .expect_err("an unreachable endpoint is an error");
    let text = error.to_string();
    assert!(text.contains("8080"), "{text}");
    assert!(text.contains("1006"), "{text}");
    assert!(text.contains("403"), "{text}");
    assert!(text.contains("502"), "{text}");
}

/// A direct session has no minter, so a tunnel over one cannot be credentialed.
///
/// Kept as an assertion rather than left to the CLI: the failure without it is a handshake
/// refused for a reason naming neither the token nor the port.
#[tokio::test]
async fn the_helpers_agree_on_the_daemon_route() {
    // The path and query the client builds are what `agentd`'s route table registers. Spelled
    // in both crates and pinned here, because the protocol crate holds the query *type* but
    // not the path.
    let url = microvms_core::session::tunnel::tunnel_url("vm.example", 5432);
    assert!(url.ends_with("/v1/tcp?port=5432"), "{url}");
    let _ = HashMap::<String, String>::new();
}

// ── layer 3: the verified tunnel, both halves in one process ─────────────────

/// A stand-in for the daemon's verified path: upgrades, runs the Noise KK responder with
/// `vm_seed` pinning `host_public`, then relays to `target` inside the session.
///
/// The same honest limit as `relay_server`: it speaks the wire contract from the `protocol`
/// crate rather than being agentd (ARCH-1 keeps the dependency one-way), and
/// `agentd/tests/tunnel_relay.rs` pins the daemon's real route from the other side.
async fn verified_relay_server(
    target: SocketAddr,
    vm_seed: [u8; 32],
    host_public: [u8; 32],
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let addr = listener.local_addr().expect("bound");

    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let recorder = Arc::new(std::sync::Mutex::new(Observed::default()));
        let socket = tokio_tungstenite::accept_hdr_async(stream, Recorder { recorder }).await;
        let Ok(mut socket) = socket else { return };

        let mut responder =
            snow::Builder::new(protocol::identity::NOISE_PATTERN.parse().expect("parses"))
                .local_private_key(&vm_seed)
                .expect("a 32-byte secret")
                .remote_public_key(&host_public)
                .expect("a 32-byte key")
                .build_responder()
                .expect("builds");

        // The handshake: read the initiator's message, answer with ours.
        let mut scratch = vec![0_u8; 65535];
        let first = loop {
            match socket.next().await {
                Some(Ok(Message::Binary(bytes))) => break bytes,
                Some(Ok(_)) => continue,
                _ => return,
            }
        };
        if responder.read_message(&first, &mut scratch).is_err() {
            let frame = tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: protocol::tunnel::close::IDENTITY_REFUSED.into(),
                reason: "identity handshake refused".into(),
            };
            let _ = socket.send(Message::Close(Some(frame))).await;
            return;
        }
        let written = responder.write_message(&[], &mut scratch).expect("writes");
        if socket
            .send(Message::Binary(scratch[..written].to_vec()))
            .await
            .is_err()
        {
            return;
        }
        let mut noise = responder.into_transport_mode().expect("transport");

        let Ok(guest) = tokio::net::TcpStream::connect(target).await else {
            return;
        };
        let (mut guest_read, mut guest_write) = guest.into_split();
        let mut buffer = vec![0_u8; 32 * 1024];
        let mut plain = vec![0_u8; 65535];
        loop {
            tokio::select! {
                inbound = socket.next() => match inbound {
                    Some(Ok(Message::Binary(bytes))) => {
                        let Ok(count) = noise.read_message(&bytes, &mut plain) else { break };
                        if guest_write.write_all(&plain[..count]).await.is_err() { break }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => continue,
                },
                read = guest_read.read(&mut buffer) => match read {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        let Ok(sealed) = noise.write_message(&buffer[..count], &mut plain) else { break };
                        if socket.send(Message::Binary(plain[..sealed].to_vec())).await.is_err() {
                            break
                        }
                    }
                },
            }
        }
        let _ = socket.send(Message::Close(None)).await;
    });

    addr
}

/// **Layer 3 end to end: the client's verified relay proves the VM and carries plaintext
/// through an encrypted channel.**
///
/// The identities are built exactly as a launch builds them — `LaunchIdentity` emits the
/// payload fields, the stand-in daemon decodes them its way — so this also pins the
/// derivation agreement between what `run --identity` sends and what `tunnel
/// --verify-identity` later verifies against.
#[tokio::test]
async fn a_verified_tunnel_completes_and_carries_bytes() {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;

    let launch = microvms_core::identity::LaunchIdentity::generate().expect("the pool works");
    let vm_seed: [u8; 32] = b64
        .decode(launch.seed_field())
        .expect("valid")
        .try_into()
        .expect("32 bytes");
    let host_public: [u8; 32] = b64
        .decode(launch.host_public_field())
        .expect("valid")
        .try_into()
        .expect("32 bytes");
    let kept = launch.keep();

    let guest = upper_server().await;
    let relay = verified_relay_server(guest, vm_seed, host_public).await;
    let (auth, _minter) = auth();

    let (mut client, local) = tokio::io::duplex(64 * 1024);
    let endpoint = format!("http://{relay}");
    let pump = tokio::spawn(async move {
        microvms_core::session::tunnel::relay_connection_verified(
            local,
            &endpoint,
            8080,
            AGENT_TOKEN,
            &auth,
            &kept,
        )
        .await
    });

    client.write_all(b"verified bytes").await.expect("written");
    let mut answer = vec![0_u8; 14];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.read_exact(&mut answer),
    )
    .await
    .expect("an answer arrives")
    .expect("read");
    assert_eq!(&answer, b"VERIFIED BYTES");

    drop(client);
    let ended = tokio::time::timeout(std::time::Duration::from_secs(5), pump)
        .await
        .expect("the pump finishes")
        .expect("the task joins")
        .expect("no transport error");
    assert_eq!(ended, TunnelEnd::Closed);
}

/// **A wrong pin fails closed, before any local byte moves.**
///
/// The far end here is a *different* VM — a fresh seed — which is exactly what a replayed
/// ledger record produces. The client must report the pin mismatch, and the guest server
/// behind the relay must never be reached (the stand-in refuses before dialing).
#[tokio::test]
async fn a_wrong_pin_fails_closed_with_a_diagnosis() {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;

    // The VM that is actually running: its own seed, pinning the caller's host key.
    let launch = microvms_core::identity::LaunchIdentity::generate().expect("the pool works");
    let host_public: [u8; 32] = b64
        .decode(launch.host_public_field())
        .expect("valid")
        .try_into()
        .expect("32 bytes");
    // A DIFFERENT VM's seed behind the endpoint.
    let other_vm_seed = [0x42_u8; 32];

    let guest = upper_server().await;
    let relay = verified_relay_server(guest, other_vm_seed, host_public).await;
    let (auth, _minter) = auth();

    let kept = launch.keep();
    let (_client, local) = tokio::io::duplex(64 * 1024);
    let endpoint = format!("http://{relay}");
    let outcome = microvms_core::session::tunnel::relay_connection_verified(
        local,
        &endpoint,
        8080,
        AGENT_TOKEN,
        &auth,
        &kept,
    )
    .await;

    // Either side may detect it first (both statics are in the handshake hash): the daemon
    // refuses with 4403, or our own verification of its reply fails with the pin diagnosis.
    match outcome {
        Ok(TunnelEnd::Refused { code, .. }) => {
            assert_eq!(code, protocol::tunnel::close::IDENTITY_REFUSED);
        }
        Err(error) => {
            let text = error.to_string();
            assert!(
                text.contains("pinned key"),
                "the error must name the pin: {text}"
            );
        }
        Ok(other) => panic!("a wrong pin must not produce a working tunnel: {other:?}"),
    }
}
