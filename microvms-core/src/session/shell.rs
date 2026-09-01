// SPDX-License-Identifier: Apache-2.0
//! The client half of `microvm shell`: a real PTY carried over the endpoint's shell
//! WebSocket. Issue #69, Option B — a thin client over the platform's session, not a
//! line-mode wrapper over exec.
//!
//! Everything protocol-shaped in this module is measured, not inferred
//! (`docs/PLATFORM.md`, "The shell endpoint is a real PTY over a WebSocket", 2026-08-15):
//!
//! * The endpoint is the VM's ordinary endpoint URL, opened as a WebSocket with **two**
//!   subprotocols — the marker and the authentication value carrying the shell JWE.
//!   There is **no port subprotocol**: the shell is not a port, and offering
//!   `lambda-microvms.port.<n>` neither helps nor hurts, so this client offers exactly
//!   what suffices.
//! * The server sends one **text** frame on connect
//!   (`{"type":"session_init","session_id":"<uuid>"}`), then **binary** frames carry raw
//!   terminal bytes both ways. Client input is raw keystrokes: `0x03` raises SIGINT in
//!   the guest, `"exit\n"` ends the shell.
//! * A resize is a JSON **control frame**, `{"type":"resize","cols":120,"rows":40}`.
//!   Before the first one the guest's `stty size` reports `0 0`, which is why
//!   [`run_shell`] sends the caller's initial size unprompted.
//! * A clean exit is close code **1000**, reason `shell exited`.
//!
//! # Sharp edge 1: a malformed control frame becomes keystrokes, not an error
//!
//! The platform does not reject an unrecognized control frame — it injects it into the
//! shell as literal input. Measured: `{"type":"window_size",...}` produced
//! `bash: type:window_sizestty: command not found` in the session. A typo in a control
//! message corrupts the terminal instead of erroring, so **every** outbound text frame
//! goes through [`validate_control_frame`] before it is sent, and the only constructor
//! for one is [`resize_frame`]. A frame the validator does not recognize is refused on
//! this side of the wire, where the failure has a message instead of a corrupted prompt.
//!
//! # Sharp edge 2: there is no exit-status channel, and this module does not invent one
//!
//! The shell's exit is a WebSocket close, full stop. There are no exec ids, no separated
//! stdout/stderr, and no exit codes — which is precisely why this is its own surface and
//! not a method on the exec path (TRAP-11). [`ShellEnd`] reports how the *transport*
//! ended and nothing about the last command's status; a caller who needs a command's
//! status must ask the shell itself (`echo $?`) and read the byte stream. Anything else
//! would be a fabricated exit code.
//!
//! # The token gates the handshake, not the session
//!
//! The shell JWE comes from `CreateMicrovmShellAuthToken`
//! ([`crate::control::ControlPlane::mint_shell_auth_token`]) and is spent once, on the
//! upgrade. An established session outlives the token's expiry, so there is no refresh
//! loop here — one mint, one handshake, one PTY.

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::{Message, protocol::CloseFrame};

use crate::error::{Error, ErrorKind};
use crate::session::proxy::{WS_AUTH_SUBPROTOCOL_PREFIX, WS_SUBPROTOCOL};

/// The close reason the platform sends when the shell process exits.
///
/// Recorded for diagnostics rather than matched on: the **code** (1000) is what
/// classifies a close as the shell exiting, because a reason string is free text the
/// platform could reword without an API change.
pub const SHELL_EXITED_REASON: &str = "shell exited";

/// Bytes read from the local input per relayed frame.
///
/// The tunnel's chunk, for the tunnel's reason: matching the daemon-side relay keeps
/// either side from being the one that fragments a stream. Interactive input is single
/// keystrokes almost always, so the size only matters for a paste.
pub const SHELL_CHUNK_BYTES: usize = 64 * 1024;

/// How a shell session ended. A value rather than an error, because every variant is a
/// session that did its transport job — see the module docs on the missing exit-status
/// channel before adding anything status-shaped here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShellEnd {
    /// The platform closed with code 1000: the shell process exited. Says nothing about
    /// the last command's status — there is no channel that could.
    Exited { reason: String },
    /// The local input reached EOF, so this side hung up on a shell that was still
    /// running. Distinct from [`ShellEnd::Exited`] because "the user closed their end"
    /// and "the shell ended" are different facts to report.
    LocalClosed,
    /// The session ended some other way: a dropped connection, a proxy-side refusal, or
    /// a close code this system does not originate. 1006 with no reason is what every
    /// endpoint-proxy failure collapses to (the tunnel's measurement holds here too).
    Disconnected { code: u16, reason: String },
}

/// What [`run_shell`] hands back: the session's identity and how it ended.
#[derive(Clone, Debug)]
pub struct ShellOutcome {
    /// The `session_id` from the `session_init` frame, when one arrived. `None` means
    /// the stream started without one — protocol drift worth surfacing, not an error
    /// worth killing a working session over.
    pub session_id: Option<String>,
    pub end: ShellEnd,
}

/// The `wss://` URL for a VM's shell: the endpoint itself, no path.
///
/// The same scheme rule as [`crate::session::tunnel::tunnel_url`], for the same reason:
/// the platform hands back a bare hostname, and defaulting a missing prefix to plaintext
/// would put the shell credential on the wire in clear. Explicit `http://`/`ws://` stays
/// plaintext for the local-daemon test case.
pub fn shell_url(endpoint: &str) -> String {
    let host = endpoint
        .trim_end_matches('/')
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("wss://")
        .trim_start_matches("ws://");
    let scheme = if endpoint.starts_with("http://") || endpoint.starts_with("ws://") {
        "ws"
    } else {
        "wss"
    };
    format!("{scheme}://{host}/")
}

/// The two subprotocols a shell handshake offers: the marker and the credential.
///
/// Two, not three. Every other WebSocket through the proxy adds
/// `lambda-microvms.port.<n>`, and the shell deliberately does not: the token names no
/// port (`CreateMicrovmShellAuthToken` has no `allowedPorts` member) and the measured
/// handshake succeeds without one. Offering the port value anyway would work today —
/// measured as "neither helps nor hurts" — and would teach every reader that the shell
/// is a port, which is the misunderstanding this crate keeps having to correct.
pub fn shell_subprotocols(shell_jwe: &str) -> [String; 2] {
    [
        WS_SUBPROTOCOL.to_string(),
        format!("{WS_AUTH_SUBPROTOCOL_PREFIX}{shell_jwe}"),
    ]
}

/// A resize control frame, validated before it leaves this function.
///
/// The one control frame the platform speaks, built here and nowhere else. The
/// validation call is not redundant with construction: it is the guard that stays in
/// the path if this function grows a second frame shape, and it is the same check
/// [`run_shell`] applies to anything it sends — so a frame that would be injected into
/// the shell as keystrokes (sharp edge 1) cannot be produced by this crate at all.
pub fn resize_frame(cols: u16, rows: u16) -> Result<String, Error> {
    if cols == 0 || rows == 0 {
        // A 0×0 resize is what the guest already reports before any resize arrives;
        // sending one would set the size every full-screen program treats as "unknown".
        // The caller passing 0 read a size from something that is not a terminal.
        return Err(Error::new(
            ErrorKind::InvalidArg,
            format!(
                "a terminal cannot be resized to {cols}x{rows}: a zero dimension is the \
                 'no size known' value, not a size. Reading the size from something that \
                 is not a terminal returns this."
            ),
        ));
    }
    let frame = format!("{{\"type\":\"resize\",\"cols\":{cols},\"rows\":{rows}}}");
    validate_control_frame(&frame)?;
    Ok(frame)
}

/// Refuses any outbound text frame the platform would not recognize as a control frame.
///
/// The platform's failure mode for an unrecognized control frame is to type it into the
/// shell (sharp edge 1, measured: `{"type":"window_size",...}` became literal keystrokes
/// and a corrupted prompt). So the rule here is a strict allowlist of what the platform
/// is measured to accept — exactly `{"type":"resize","cols":<n>,"rows":<n>}` — and
/// anything else is refused client-side, where the failure is a sentence rather than
/// garbage in someone's terminal.
pub fn validate_control_frame(text: &str) -> Result<(), Error> {
    let refuse = |why: String| {
        Error::new(
            ErrorKind::InvalidArg,
            format!(
                "refusing to send a control frame the platform would not recognize: {why}. An \
                 unrecognized control frame is not rejected — it is injected into the shell as \
                 literal keystrokes (docs/PLATFORM.md, 2026-08-15). Frame: {text}"
            ),
        )
    };

    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|err| refuse(format!("it is not JSON ({err})")))?;
    let object = value
        .as_object()
        .ok_or_else(|| refuse("it is JSON but not an object".to_string()))?;

    match object.get("type").and_then(serde_json::Value::as_str) {
        Some("resize") => {}
        Some(other) => {
            return Err(refuse(format!(
                "\"{other}\" is not a control type the platform speaks; \"resize\" is the \
                 only one"
            )));
        }
        None => return Err(refuse("it has no string \"type\" member".to_string())),
    }
    for key in object.keys() {
        if key != "type" && key != "cols" && key != "rows" {
            return Err(refuse(format!(
                "\"{key}\" is not a member of a resize frame"
            )));
        }
    }
    for dimension in ["cols", "rows"] {
        let looks_sized = object
            .get(dimension)
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|size| size >= 1 && size <= u64::from(u16::MAX));
        if !looks_sized {
            return Err(refuse(format!(
                "\"{dimension}\" must be an integer in 1..=65535"
            )));
        }
    }
    Ok(())
}

/// Opens the shell and relays raw terminal bytes until one side ends the session.
///
/// `input`/`output` are any async byte streams, so the CLI drives this with the process's
/// raw-mode stdin/stdout and a test drives it with pipes. `resize` is a watch channel of
/// `(cols, rows)`: the value at entry is sent immediately — the guest reports `0 0` until
/// something does — and every change afterwards becomes one validated control frame.
///
/// Output is flushed per frame, because the reader on the other side of `output` is a
/// human watching a prompt: a byte held in a buffer is a keystroke that appears to have
/// been eaten.
pub async fn run_shell<I, O>(
    mut input: I,
    mut output: O,
    endpoint: &str,
    shell_jwe: &str,
    mut resize: tokio::sync::watch::Receiver<(u16, u16)>,
) -> Result<ShellOutcome, Error>
where
    I: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
{
    let url = shell_url(endpoint);
    let mut request = url.as_str().into_client_request().map_err(|err| {
        Error::new(
            ErrorKind::InvalidArg,
            format!("{url} is not a usable WebSocket URL: {err}"),
        )
    })?;
    {
        // The two platform values as one comma-separated list, exactly as the tunnel
        // offers its three. No `authorization` header: the shell terminates at the
        // platform's PTY, not at the daemon, so there is no bearer check on this path
        // and the shell JWE in the subprotocol is the whole credential.
        let offered = shell_subprotocols(shell_jwe);
        request.headers_mut().insert(
            "sec-websocket-protocol",
            offered.join(", ").parse().map_err(|err| {
                Error::new(
                    ErrorKind::Unexpected,
                    format!("the shell subprotocols are not a legal header value: {err}"),
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
                        "the shell handshake failed: {err}. Every endpoint-proxy WebSocket \
                         failure is close code 1006 with no reason, so if this names nothing: \
                         the shell token may have expired before the connect (it gates the \
                         handshake, not the session), or the endpoint may be wrong. A VM \
                         launched without SHELL_INGRESS fails earlier, at the token mint."
                    ),
                )
            })?;

    // The caller's size, sent before any keystroke: until a resize arrives the guest's
    // line discipline believes the terminal is 0x0, and the first full-screen program
    // the user starts would render into that. `borrow_and_update` rather than `borrow` +
    // a later `mark_unchanged`: the caller's poller is already publishing into this
    // channel, and clearing the change flag in a separate step would eat a resize that
    // landed in between — the stored value would then be what future sizes are compared
    // against, and the missed resize would never be re-sent.
    let (cols, rows) = *resize.borrow_and_update();
    if let Ok(frame) = resize_frame(cols, rows) {
        // A (0, 0) initial value is not an error here: the caller read the size from
        // something that is not a terminal (a pipe, a CI runner), and the session is
        // still usable — the guest just keeps its size-unknown default.
        socket
            .send(Message::Text(frame.into()))
            .await
            .map_err(|err| {
                Error::new(
                    ErrorKind::Unexpected,
                    format!("the shell closed before the initial resize could be sent: {err}"),
                )
            })?;
    }

    let mut session_id: Option<String> = None;
    let mut buffer = vec![0_u8; SHELL_CHUNK_BYTES];
    loop {
        tokio::select! {
            // Raw keystrokes from the local terminal, framed as binary, sent as-is.
            read = input.read(&mut buffer) => match read {
                Ok(0) => {
                    // The local side is done sending. A close frame rather than a drop,
                    // so the platform tears the PTY down instead of holding a session
                    // whose keyboard has vanished.
                    let _ = socket.send(Message::Close(None)).await;
                    let _ = output.flush().await;
                    return Ok(ShellOutcome { session_id, end: ShellEnd::LocalClosed });
                }
                Ok(count) => {
                    if socket
                        .send(Message::Binary(buffer[..count].to_vec().into()))
                        .await
                        .is_err()
                    {
                        let _ = output.flush().await;
                        return Ok(ShellOutcome {
                            session_id,
                            end: ShellEnd::Disconnected {
                                code: 1006,
                                reason: "the connection dropped while sending input".to_string(),
                            },
                        });
                    }
                }
                Err(err) => {
                    return Err(Error::new(
                        ErrorKind::Unexpected,
                        format!("reading the local terminal failed: {err}"),
                    ));
                }
            },
            // A size change becomes one validated control frame. The validation is the
            // sharp-edge-1 guard: nothing leaves this loop as text without passing it.
            changed = resize.changed() => {
                if changed.is_err() {
                    // The sender is gone; the session continues at its current size.
                    continue;
                }
                let (cols, rows) = *resize.borrow_and_update();
                let frame = match resize_frame(cols, rows) {
                    Ok(frame) => frame,
                    // A transient 0 from a size probe is not worth ending a session over.
                    Err(_) => continue,
                };
                if socket.send(Message::Text(frame.into())).await.is_err() {
                    let _ = output.flush().await;
                    return Ok(ShellOutcome {
                        session_id,
                        end: ShellEnd::Disconnected {
                            code: 1006,
                            reason: "the connection dropped while sending a resize".to_string(),
                        },
                    });
                }
            },
            // Terminal bytes from the guest, written through and flushed per frame.
            frame = socket.next() => match frame {
                Some(Ok(Message::Binary(bytes))) => {
                    if output.write_all(&bytes).await.is_err()
                        || output.flush().await.is_err()
                    {
                        let _ = socket.send(Message::Close(None)).await;
                        return Ok(ShellOutcome { session_id, end: ShellEnd::LocalClosed });
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    // The one modeled text frame is the greeting. Anything after it is
                    // protocol drift; written through rather than swallowed, because
                    // hiding bytes from a terminal user is worse than showing them
                    // something unexpected.
                    if session_id.is_none()
                        && let Some(id) = parse_session_init(&text)
                    {
                        session_id = Some(id);
                        continue;
                    }
                    if output.write_all(text.as_bytes()).await.is_err()
                        || output.flush().await.is_err()
                    {
                        let _ = socket.send(Message::Close(None)).await;
                        return Ok(ShellOutcome { session_id, end: ShellEnd::LocalClosed });
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    let _ = output.flush().await;
                    return Ok(ShellOutcome {
                        session_id,
                        end: classify_close(frame.as_ref()),
                    });
                }
                Some(Ok(_)) => continue,
                Some(Err(err)) => {
                    let _ = output.flush().await;
                    return Ok(ShellOutcome {
                        session_id,
                        end: ShellEnd::Disconnected {
                            code: 1006,
                            reason: format!("the connection failed mid-session: {err}"),
                        },
                    });
                }
                None => {
                    let _ = output.flush().await;
                    return Ok(ShellOutcome {
                        session_id,
                        end: ShellEnd::Disconnected {
                            code: 1006,
                            reason: "the connection ended with no close frame".to_string(),
                        },
                    });
                }
            },
        }
    }
}

/// Reads a `session_init` greeting, or `None` when the text is something else.
///
/// Deliberately narrow: only `{"type":"session_init",...}` with a string `session_id`
/// parses. A text frame that is almost-but-not a greeting goes to the caller's output,
/// per the drift stance in [`run_shell`].
fn parse_session_init(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("type")?.as_str()? != "session_init" {
        return None;
    }
    Some(value.get("session_id")?.as_str()?.to_string())
}

/// Reads the platform's close frame into a [`ShellEnd`].
///
/// Code 1000 is the shell exiting — the **code** classifies, the reason is carried for
/// display. A close with no frame at all is *not* the measured clean exit, so it is
/// reported as a disconnect (1005, "no status received") rather than dressed up as one:
/// the caller cannot know whether the shell ended or the proxy hung up.
fn classify_close(frame: Option<&CloseFrame>) -> ShellEnd {
    let Some(frame) = frame else {
        return ShellEnd::Disconnected {
            code: 1005,
            reason: "the session closed with no close frame".to_string(),
        };
    };
    let code: u16 = frame.code.into();
    if code == 1000 {
        return ShellEnd::Exited {
            reason: frame.reason.to_string(),
        };
    }
    ShellEnd::Disconnected {
        code,
        reason: frame.reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_endpoint_host_becomes_wss_at_the_root() {
        // The tunnel's downgrade rule, applied to the shell: a missing prefix must not
        // put the shell credential on a plaintext socket.
        assert_eq!(shell_url("vm-abc.example.aws"), "wss://vm-abc.example.aws/");
        assert_eq!(
            shell_url("https://vm-abc.example.aws/"),
            "wss://vm-abc.example.aws/"
        );
        // Explicit plaintext is a caller saying they know — the local test case.
        assert_eq!(shell_url("http://127.0.0.1:9000"), "ws://127.0.0.1:9000/");
    }

    /// Two subprotocols, and **no port value** — the absence is the measured protocol,
    /// not an omission. The marker stays aliased to the protocol crate's constant.
    ///
    /// **Falsification** — watched fail 2026-08-31: appending
    /// `lambda-microvms.port.9000` to [`shell_subprotocols`] fails the length assertion,
    /// and the no-port scan fails with the offending value in the message.
    #[test]
    fn the_shell_offers_two_subprotocols_and_no_port() {
        let offered = shell_subprotocols("opaque-jwe");
        assert_eq!(offered.len(), 2);
        assert_eq!(offered[0], "lambda-microvms");
        assert_eq!(offered[1], "lambda-microvms.authentication.opaque-jwe");
        for value in &offered {
            assert!(
                !value.starts_with("lambda-microvms.port."),
                "the shell is not a port, so no handshake value may name one: {value}"
            );
        }
    }

    /// The resize frame, byte for byte the measured shape. Pinned as a literal because
    /// the platform parses this with something that types unrecognized frames into the
    /// shell — the frame's exact spelling is the contract.
    #[test]
    fn the_resize_frame_is_the_measured_shape_byte_for_byte() {
        assert_eq!(
            resize_frame(120, 40).expect("a legal size"),
            r#"{"type":"resize","cols":120,"rows":40}"#
        );
    }

    /// A zero dimension is refused with the reason: 0×0 is the guest's "no size known"
    /// state, not a size.
    #[test]
    fn a_zero_dimension_is_refused() {
        for (cols, rows) in [(0, 40), (120, 0), (0, 0)] {
            let error = resize_frame(cols, rows).expect_err("zero is not a size");
            assert_eq!(error.kind(), ErrorKind::InvalidArg);
        }
    }

    /// **Sharp edge 1, as a guard.** The exact frame the measurement watched corrupt a
    /// session — `{"type":"window_size",...}` — is refused client-side, along with every
    /// other shape the platform would type into the shell as keystrokes.
    ///
    /// **Falsification** — watched fail 2026-08-31: making `validate_control_frame`
    /// accept any object with a `"type"` member passes the `window_size` case straight
    /// through, and this test fails on it.
    #[test]
    fn a_control_frame_the_platform_would_not_recognize_is_refused_before_the_wire() {
        // The measured corruption, verbatim from docs/PLATFORM.md.
        let measured = r#"{"type":"window_size","cols":120,"rows":40}"#;
        let error = validate_control_frame(measured).expect_err("window_size corrupts");
        let message = error.to_string();
        assert!(message.contains("window_size"), "{message}");
        assert!(
            message.contains("literal keystrokes"),
            "the message must say what the platform does with it: {message}"
        );

        // The rest of the refusal surface: not JSON, not an object, no type, an extra
        // member, and a dimension that is missing, non-numeric, zero, or oversized.
        for bad in [
            "resize 120 40",
            "[1,2,3]",
            r#"{"cols":120,"rows":40}"#,
            r#"{"type":"resize","cols":120,"rows":40,"pad":1}"#,
            r#"{"type":"resize","cols":120}"#,
            r#"{"type":"resize","cols":"120","rows":40}"#,
            r#"{"type":"resize","cols":0,"rows":40}"#,
            r#"{"type":"resize","cols":120,"rows":65536}"#,
        ] {
            assert!(validate_control_frame(bad).is_err(), "must refuse: {bad}");
        }

        // And the one legal frame still passes, or the guard is a wall rather than a door.
        validate_control_frame(r#"{"type":"resize","cols":120,"rows":40}"#)
            .expect("the measured frame is legal");
    }

    /// Close classification: 1000 is the shell exiting whatever the reason says; any
    /// other code — and a missing frame — is a disconnect, never dressed up as an exit.
    #[test]
    fn only_code_1000_is_the_shell_exiting() {
        let clean = CloseFrame {
            code: 1000.into(),
            reason: SHELL_EXITED_REASON.into(),
        };
        assert_eq!(
            classify_close(Some(&clean)),
            ShellEnd::Exited {
                reason: "shell exited".to_string()
            }
        );

        // The reason is display material, not the classifier: a reworded platform
        // string must not turn clean exits into failures.
        let reworded = CloseFrame {
            code: 1000.into(),
            reason: "session complete".into(),
        };
        assert!(matches!(
            classify_close(Some(&reworded)),
            ShellEnd::Exited { .. }
        ));

        // No frame is not the measured clean exit, and saying "exited" would fake a
        // fact nobody has.
        assert_eq!(
            classify_close(None),
            ShellEnd::Disconnected {
                code: 1005,
                reason: "the session closed with no close frame".to_string()
            }
        );

        let dropped = CloseFrame {
            code: 1006.into(),
            reason: String::new().into(),
        };
        assert!(matches!(
            classify_close(Some(&dropped)),
            ShellEnd::Disconnected { code: 1006, .. }
        ));
    }

    /// The greeting parses, and near-greetings do not: a text frame that is almost a
    /// `session_init` goes to the terminal rather than being silently eaten.
    #[test]
    fn the_session_init_greeting_parses_and_near_misses_do_not() {
        assert_eq!(
            parse_session_init(r#"{"type":"session_init","session_id":"abc-123"}"#),
            Some("abc-123".to_string())
        );
        for near_miss in [
            r#"{"type":"session_start","session_id":"abc"}"#,
            r#"{"type":"session_init"}"#,
            r#"{"type":"session_init","session_id":7}"#,
            "not json",
        ] {
            assert_eq!(parse_session_init(near_miss), None, "{near_miss}");
        }
    }
}
