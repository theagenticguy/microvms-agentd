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
