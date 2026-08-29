// SPDX-License-Identifier: Apache-2.0
//! The TCP relay, end to end over a real WebSocket against a real TCP server.
//!
//! Every property this file asserts is only observable through a live upgrade: the router's
//! `oneshot` harness cannot carry frames, and a unit test of the relay loop would be
//! asserting against its own fake sink. So this binds the daemon on loopback, opens a real
//! `ws://` connection with `tokio-tungstenite`, and puts a real TCP echo server on the far
//! side — which is the same shape the guest actually has.
//!
//! The relay's whole contract is bytes and close codes. Bytes, because it carries somebody
//! else's protocol and a single corrupted or reordered byte breaks it silently. Close codes,
//! because on the endpoint path every failure a caller can observe is 1006 with no reason
//! (measured, `docs/PLATFORM.md`), so the codes this relay originates are the only diagnostic
//! a `microvm tunnel` user will ever get.

use std::collections::HashMap;
use std::net::SocketAddr;

use agentd::config::Config;
use agentd::routes;
use agentd::state::AppState;
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::{Message, protocol::CloseFrame};

const TOKEN: &str = "relay-probe-token";

/// Starts the daemon on an ephemeral loopback port and returns its address.
async fn daemon() -> SocketAddr {
    let state = AppState::new(Config::default());
    state.bootstrap(TOKEN.as_bytes(), HashMap::new());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let addr = listener.local_addr().expect("bound");
    tokio::spawn(async move {
        let _ = axum::serve(listener, routes::app(state)).await;
    });
    addr
}

/// A TCP server that upper-cases whatever it receives, so an echo cannot pass by accident.
///
/// Transforming rather than echoing is deliberate: a relay that looped bytes back on the
/// caller's own socket without ever reaching the guest would pass an echo test.
async fn upper_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let addr = listener.local_addr().expect("bound");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 64 * 1024];
                loop {
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(count) => {
                            let upper: Vec<u8> = buffer[..count]
                                .iter()
                                .map(|b| b.to_ascii_uppercase())
                                .collect();
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

/// Opens a tunnel WebSocket to `port`, with a bearer token unless `token` is `None`.
async fn open_tunnel(
    daemon: SocketAddr,
    port: u16,
    token: Option<&str>,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    tokio_tungstenite::tungstenite::Error,
> {
    let mut request = format!("ws://{daemon}/v1/tcp?port={port}")
        .into_client_request()
        .expect("a well-formed request");
    if let Some(token) = token {
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {token}").parse().expect("a header value"),
        );
    }
    let (socket, _) = tokio_tungstenite::connect_async(request).await?;
    Ok(socket)
}

/// The close code and reason a socket ended with, draining any data frames first.
async fn close_outcome(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
) -> Option<(u16, String)> {
    while let Some(message) = socket.next().await {
        match message {
            Ok(Message::Close(Some(CloseFrame { code, reason }))) => {
                return Some((code.into(), reason.to_string()));
            }
            Ok(Message::Close(None)) => return Some((1000, String::new())),
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
    None
}

/// **The relay carries bytes to a real server and back.**
///
/// The upper-casing server is what makes this a relay test rather than an echo test: the
/// transformed bytes could only have come from the far side.
#[tokio::test]
async fn bytes_reach_the_guest_server_and_its_answer_comes_back() {
    let server = upper_server().await;
    let daemon = daemon().await;

    let mut socket = open_tunnel(daemon, server.port(), Some(TOKEN))
        .await
        .expect("the upgrade succeeds");

    socket
        .send(Message::Binary(b"hello relay".to_vec()))
        .await
        .expect("the frame is sent");

    let answer = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .expect("an answer arrives")
        .expect("a message")
        .expect("not an error");
    match answer {
        Message::Binary(bytes) => assert_eq!(&bytes[..], b"HELLO RELAY"),
        other => panic!("expected a binary frame, got {other:?}"),
    }
}

/// **Binary payloads survive byte-exact, including the bytes utf-8 would corrupt.**
///
/// `0x00` and `0xFF` are the load-bearing cases: a relay that round-tripped its payload
/// through a `String` anywhere would return the right length and the wrong bytes, and an
/// ascii-only test cannot tell the two apart. This is the guest-side half of the same
/// property measured against the platform proxy on 2026-08-29.
#[tokio::test]
async fn a_non_utf8_payload_survives_byte_exact() {
    // A server that returns its input unchanged, so the assertion is about fidelity alone.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let server = listener.local_addr().expect("bound");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("one connection");
        let mut buffer = vec![0_u8; 4096];
        let count = socket.read(&mut buffer).await.expect("bytes arrive");
        socket
            .write_all(&buffer[..count])
            .await
            .expect("the echo is written");
    });

    let daemon = daemon().await;
    let mut socket = open_tunnel(daemon, server.port(), Some(TOKEN))
        .await
        .expect("the upgrade succeeds");

    let payload: Vec<u8> = vec![0x00, 0xff, 0xfe, 0x80, 0x7f, 0x00, 0xc0, 0x80];
    socket
        .send(Message::Binary(payload.clone()))
        .await
        .expect("sent");

    let answer = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .expect("an answer arrives")
        .expect("a message")
        .expect("not an error");
    match answer {
        Message::Binary(bytes) => assert_eq!(
            bytes.to_vec(),
            payload,
            "the relay corrupted a non-utf8 payload"
        ),
        other => panic!("expected binary, got {other:?}"),
    }
}

/// **A payload larger than one relay chunk arrives whole and in order.**
///
/// The relay reads 64 KiB at a time, so a 200 KiB stream crosses that boundary several
/// times. Order is the assertion that matters: a relay that spawned a task per read could
/// deliver chunks out of order and still deliver every byte.
#[tokio::test]
async fn a_payload_larger_than_one_chunk_arrives_whole_and_in_order() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let server = listener.local_addr().expect("bound");
    let total = 200 * 1024;
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("one connection");
        // Send a deterministic ramp so a reordering is visible as a value mismatch rather
        // than only as a length mismatch.
        let payload: Vec<u8> = (0..total).map(|index| (index % 251) as u8).collect();
        let _ = socket.write_all(&payload).await;
        let _ = socket.shutdown().await;
    });

    let daemon = daemon().await;
    let mut socket = open_tunnel(daemon, server.port(), Some(TOKEN))
        .await
        .expect("the upgrade succeeds");

    let mut received = Vec::new();
    while received.len() < total {
        let message = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
            .await
            .expect("a frame arrives")
            .expect("a message")
            .expect("not an error");
        match message {
            Message::Binary(bytes) => received.extend_from_slice(&bytes),
            Message::Close(_) => break,
            _ => continue,
        }
    }

    assert_eq!(received.len(), total, "the relay lost bytes");
    let expected: Vec<u8> = (0..total).map(|index| (index % 251) as u8).collect();
    assert_eq!(received, expected, "the relay reordered or corrupted bytes");
}

/// **A dead guest port closes with 4502, not silence.**
///
/// The one diagnostic a tunnel user gets. Bound-then-dropped rather than a guessed port, so
/// the port is provably closed rather than probably closed.
#[tokio::test]
async fn a_dead_guest_port_closes_with_no_listener_and_names_the_port() {
    let held = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let dead = held.local_addr().expect("bound").port();
    drop(held);

    let daemon = daemon().await;
    let mut socket = open_tunnel(daemon, dead, Some(TOKEN))
        .await
        .expect("the upgrade succeeds even though the dial will fail");

    let (code, reason) = close_outcome(&mut socket).await.expect("a close frame");
    assert_eq!(code, protocol::tunnel::close::NO_LISTENER);
    assert!(reason.contains(&dead.to_string()), "{reason}");
    assert!(reason.contains("listening"), "{reason}");
}

/// **`?port=0` closes with 4400 rather than being dialled.**
#[tokio::test]
async fn port_zero_closes_with_the_bad_port_code() {
    let daemon = daemon().await;
    let mut socket = open_tunnel(daemon, 0, Some(TOKEN))
        .await
        .expect("the upgrade succeeds");

    let (code, reason) = close_outcome(&mut socket).await.expect("a close frame");
    assert_eq!(code, protocol::tunnel::close::BAD_PORT);
    assert!(reason.contains("port 0"), "{reason}");
}

/// **The relay is unreachable without the agent token.**
///
/// The security boundary, asserted rather than assumed. A relay reachable unauthenticated
/// would let the workload inside the VM open connections through the daemon's identity —
/// which is the confusion `docs/TRUST.md` exists to prevent.
///
/// **Falsification:** move `/v1/tcp` from `Auth::Bearer` to `Auth::Open` in `surface_docs`
/// and this handshake succeeds.
#[tokio::test]
async fn the_relay_refuses_an_unauthenticated_upgrade() {
    let server = upper_server().await;
    let daemon = daemon().await;

    let error = open_tunnel(daemon, server.port(), None)
        .await
        .expect_err("an unauthenticated upgrade must be refused");
    // tungstenite surfaces a non-101 as an HTTP error carrying the status.
    match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(
                response.status(),
                401,
                "the refusal must be an auth failure"
            );
        }
        other => panic!("expected an HTTP 401, got {other:?}"),
    }
}

/// **A wrong token is refused the same way as no token.**
#[tokio::test]
async fn the_relay_refuses_a_wrong_token() {
    let server = upper_server().await;
    let daemon = daemon().await;

    let error = open_tunnel(daemon, server.port(), Some("not-the-token"))
        .await
        .expect_err("a wrong token must be refused");
    match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(response.status(), 401);
        }
        other => panic!("expected an HTTP 401, got {other:?}"),
    }
}

/// **A caller's close ends the tunnel cleanly rather than as a failure.**
///
/// A finished tunnel is a success. A caller that read a completed request as an error would
/// retry work that already happened.
#[tokio::test]
async fn a_callers_close_ends_the_tunnel_at_code_1000() {
    let server = upper_server().await;
    let daemon = daemon().await;

    let mut socket = open_tunnel(daemon, server.port(), Some(TOKEN))
        .await
        .expect("the upgrade succeeds");
    socket.close(None).await.expect("the close is sent");

    // Drain to completion: the relay answers a close with a close.
    while let Some(message) = socket.next().await {
        match message {
            Ok(Message::Close(Some(frame))) => {
                let code: u16 = frame.code.into();
                assert_eq!(code, protocol::tunnel::close::NORMAL, "{}", frame.reason);
                return;
            }
            Ok(_) | Err(_) => continue,
        }
    }
}

/// **The guest closing its side ends the tunnel, so a client is not left hanging.**
#[tokio::test]
async fn the_guest_hanging_up_closes_the_tunnel() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let server = listener.local_addr().expect("bound");
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("one connection");
        // Accept and immediately hang up, which is what a server refusing a protocol does.
        drop(socket);
    });

    let daemon = daemon().await;
    let mut socket = open_tunnel(daemon, server.port(), Some(TOKEN))
        .await
        .expect("the upgrade succeeds");

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        close_outcome(&mut socket),
    )
    .await
    .expect("the relay does not hang when the guest hangs up");
    // A clean 1000 or a dropped transport both satisfy "the client is not left waiting";
    // what would fail this test is the timeout above.
    if let Some((code, _)) = outcome {
        assert!(
            code == protocol::tunnel::close::NORMAL
                || code == protocol::tunnel::close::RELAY_FAILED,
            "unexpected close code {code}"
        );
    }
}

/// **Two tunnels to the same port are independent connections.**
///
/// The no-multiplexing decision, as a property: each WebSocket must get its own TCP
/// connection, so one tunnel's bytes can never appear in another's.
#[tokio::test]
async fn two_tunnels_do_not_share_a_connection() {
    let server = upper_server().await;
    let daemon = daemon().await;

    let mut first = open_tunnel(daemon, server.port(), Some(TOKEN))
        .await
        .expect("the first upgrade succeeds");
    let mut second = open_tunnel(daemon, server.port(), Some(TOKEN))
        .await
        .expect("the second upgrade succeeds");

    first
        .send(Message::Binary(b"first".to_vec()))
        .await
        .expect("sent");
    second
        .send(Message::Binary(b"second".to_vec()))
        .await
        .expect("sent");

    // A fn rather than a closure: an async closure taking `&mut` cannot express that the
    // borrow ends with the returned future, and the workaround is longer than the fn.
    async fn next_binary(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<TcpStream>,
        >,
    ) -> Vec<u8> {
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
                .await
                .expect("an answer arrives")
                .expect("a message")
                .expect("not an error")
            {
                Message::Binary(bytes) => return bytes.to_vec(),
                _ => continue,
            }
        }
    }

    let first_answer = next_binary(&mut first).await;
    let second_answer = next_binary(&mut second).await;
    assert_eq!(
        first_answer, b"FIRST",
        "the first tunnel got another's bytes"
    );
    assert_eq!(
        second_answer, b"SECOND",
        "the second tunnel got another's bytes"
    );
}

// ── the identity handshake, end to end ──────────────────────────────────────

/// A daemon bootstrapped with identity material derived from `vm_seed`, pinning
/// the public half of `host_seed`.
async fn identity_daemon(vm_seed: [u8; 32], host_seed: [u8; 32]) -> SocketAddr {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let host_public =
        *x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(host_seed)).as_bytes();
    let hook = protocol::hook::RunHook {
        agent_token: TOKEN.to_string(),
        env: HashMap::new(),
        identity_seed: Some(b64.encode(vm_seed)),
        identity_host_public_key: Some(b64.encode(host_public)),
    };
    let material = agentd::tunnel_identity::Material::from_payload(&hook)
        .expect("valid")
        .expect("present");

    let state = AppState::new(Config::default());
    state.bootstrap_with_identity(
        TOKEN.as_bytes(),
        HashMap::new(),
        Some(std::sync::Arc::new(material)),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let addr = listener.local_addr().expect("bound");
    tokio::spawn(async move {
        let _ = axum::serve(listener, routes::app(state)).await;
    });
    addr
}

/// Opens a tunnel with `identity=true` in the query.
async fn open_identity_tunnel(
    daemon: SocketAddr,
    port: u16,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>> {
    let mut request = format!("ws://{daemon}/v1/tcp?port={port}&identity=true")
        .into_client_request()
        .expect("a well-formed request");
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {TOKEN}").parse().expect("a header value"),
    );
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("the upgrade succeeds");
    socket
}

/// Runs the initiator's half of the Noise KK handshake over an open WebSocket.
///
/// Returns the transport state, or the daemon's close outcome when the handshake was
/// refused — which is itself an assertable result rather than a test failure.
async fn initiate(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    host_seed: [u8; 32],
    pinned_vm_public: [u8; 32],
) -> Result<snow::TransportState, Option<(u16, String)>> {
    let mut initiator =
        snow::Builder::new(protocol::identity::NOISE_PATTERN.parse().expect("parses"))
            .local_private_key(&host_seed)
            .expect("a 32-byte secret")
            .remote_public_key(&pinned_vm_public)
            .expect("a 32-byte key")
            .build_initiator()
            .expect("builds");

    let mut scratch = vec![0_u8; 65535];
    let written = initiator.write_message(&[], &mut scratch).expect("writes");
    socket
        .send(Message::Binary(scratch[..written].to_vec()))
        .await
        .expect("sent");

    // The daemon either answers with its handshake message or closes with a refusal code.
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
            .await
            .expect("an answer arrives")
        {
            Some(Ok(Message::Binary(reply))) => {
                initiator
                    .read_message(&reply, &mut scratch)
                    .expect("the daemon's reply authenticates");
                return Ok(initiator.into_transport_mode().expect("transport"));
            }
            Some(Ok(Message::Close(Some(CloseFrame { code, reason })))) => {
                return Err(Some((code.into(), reason.to_string())));
            }
            Some(Ok(Message::Close(None))) => return Err(Some((1000, String::new()))),
            Some(Ok(_)) => continue,
            Some(Err(_)) | None => return Err(None),
        }
    }
}

/// **The full layer-3 property: a verified tunnel carries encrypted bytes to a real
/// server, and the plaintext round-trips exactly.**
///
/// The upper-casing server is what proves the daemon really decrypted: the transformed
/// bytes could only have been produced from plaintext the guest side saw.
#[tokio::test]
async fn a_verified_tunnel_proves_the_vm_and_carries_encrypted_bytes() {
    let vm_seed = [7_u8; 32];
    let host_seed = [9_u8; 32];
    let vm_public =
        *x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(vm_seed)).as_bytes();

    let server = upper_server().await;
    let daemon = identity_daemon(vm_seed, host_seed).await;
    let mut socket = open_identity_tunnel(daemon, server.port()).await;

    let mut noise = initiate(&mut socket, host_seed, vm_public)
        .await
        .expect("the launching host's handshake completes");

    // Binary-unsafe bytes through the encrypted path, so a UTF-8 lossy hop cannot hide.
    let payload = b"proof: \x00\xff mixed case";
    let mut scratch = vec![0_u8; 65535];
    let written = noise
        .write_message(payload, &mut scratch)
        .expect("encrypts");
    socket
        .send(Message::Binary(scratch[..written].to_vec()))
        .await
        .expect("sent");

    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
            .await
            .expect("an answer arrives")
            .expect("a message")
            .expect("not an error")
        {
            Message::Binary(frame) => {
                let count = noise
                    .read_message(&frame, &mut scratch)
                    .expect("the answer authenticates");
                assert_eq!(
                    &scratch[..count],
                    b"PROOF: \x00\xff MIXED CASE",
                    "the guest saw the plaintext and its answer came back encrypted"
                );
                return;
            }
            _ => continue,
        }
    }
}

/// **A caller who does not hold the launching host's key is refused with 4403.**
///
/// The agent token alone must not be enough: it is a bearer credential the proxy carries
/// on every request, and this test connects with a perfectly valid token and a wrong key.
#[tokio::test]
async fn a_valid_token_with_the_wrong_host_key_is_refused() {
    let vm_seed = [7_u8; 32];
    let host_seed = [9_u8; 32];
    let vm_public =
        *x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(vm_seed)).as_bytes();

    let server = upper_server().await;
    let daemon = identity_daemon(vm_seed, host_seed).await;
    let mut socket = open_identity_tunnel(daemon, server.port()).await;

    let refusal = initiate(&mut socket, [1_u8; 32], vm_public)
        .await
        .expect_err("a wrong host key must not complete");
    let (code, reason) = refusal.expect("the daemon closes with a code, not a hangup");
    assert_eq!(code, protocol::tunnel::close::IDENTITY_REFUSED);
    assert!(reason.contains("handshake"), "{reason}");
}

/// **A caller pinning the wrong VM key is refused the same way.**
///
/// The replayed-record case from #70's acceptance list: a ledger record copied from a
/// different VM carries a different public key, and the handshake must fail rather than
/// silently connect to the wrong machine.
#[tokio::test]
async fn a_pin_from_a_different_vm_is_refused() {
    let vm_seed = [7_u8; 32];
    let host_seed = [9_u8; 32];
    let other_vm_public =
        *x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from([2_u8; 32])).as_bytes();

    let server = upper_server().await;
    let daemon = identity_daemon(vm_seed, host_seed).await;
    let mut socket = open_identity_tunnel(daemon, server.port()).await;

    let refusal = initiate(&mut socket, host_seed, other_vm_public)
        .await
        .expect_err("a wrong pin must not complete");
    let (code, _) = refusal.expect("the daemon closes with a code");
    assert_eq!(code, protocol::tunnel::close::IDENTITY_REFUSED);
}

/// **`identity=true` against a VM launched without a seed is 4401, not a downgrade.**
#[tokio::test]
async fn identity_against_a_seedless_vm_is_refused_not_downgraded() {
    let server = upper_server().await;
    // The plain daemon: bootstrapped with no identity material.
    let daemon = daemon().await;
    let mut socket = open_identity_tunnel(daemon, server.port()).await;

    // No handshake to send — the daemon must close first with 4401.
    let outcome = close_outcome(&mut socket).await;
    let (code, reason) = outcome.expect("a close frame with the refusal");
    assert_eq!(code, protocol::tunnel::close::NO_IDENTITY);
    assert!(reason.contains("launched without"), "{reason}");
}

/// **The identity handshake runs before the dial: a refused caller never reaches a guest.**
///
/// Asserted by pointing the verified tunnel at a listener that records connections: after a
/// refused handshake, the listener must have seen none.
#[tokio::test]
async fn a_refused_caller_never_causes_a_guest_connection() {
    let vm_seed = [7_u8; 32];
    let host_seed = [9_u8; 32];
    let vm_public =
        *x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(vm_seed)).as_bytes();

    // A listener that counts accepts rather than serving anything.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
    let guest = listener.local_addr().expect("bound");
    let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = accepted.clone();
    tokio::spawn(async move {
        loop {
            let Ok((_socket, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });

    let daemon = identity_daemon(vm_seed, host_seed).await;
    let mut socket = open_identity_tunnel(daemon, guest.port()).await;
    let refusal = initiate(&mut socket, [1_u8; 32], vm_public).await;
    assert!(refusal.is_err(), "the wrong key must be refused");

    // The refusal already arrived, so any dial would have happened by now.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        accepted.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a refused caller must never cause a connection to the guest service"
    );
}
