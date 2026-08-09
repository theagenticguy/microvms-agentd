//! Incremental Server-Sent Events parsing, and the typed events the daemon emits.
//!
//! # Incremental because the transport picks the read boundaries
//!
//! A single `data:` line routinely arrives split across two chunks, and a parser that
//! assumes a read boundary is a frame boundary loses output at exactly the moment the
//! stream gets busy. So bytes are buffered until a blank line proves a frame is
//! complete, and a trailing partial frame stays in the buffer.
//!
//! Only the subset the daemon emits is implemented: named events with one `data:` line
//! of JSON, plus the `:` keepalive comments axum sends every fifteen seconds. `id:` and
//! `retry:` are ignored rather than half-supported — the daemon does not send them, and
//! the byte offset is the resume cursor rather than `Last-Event-ID`.
//!
//! # Hand-rolled rather than a dependency
//!
//! The daemon's SSE is two field names and three event names, and the interesting part
//! of a client's stream handling is the cursor rather than the framing. An eventsource
//! crate would bring a reconnect policy keyed on `Last-Event-ID`, which is the wrong
//! cursor: it resumes at an event, and this protocol resumes at a byte.

use base64::Engine as _;

use crate::error::{Error, ErrorKind, WireKind};

/// One complete SSE frame: its event name and its raw `data:` text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub event: String,
    pub data: String,
}

/// The longest frame terminator, in bytes. See [`find_frame_end`].
const MAX_TERMINATOR_LEN: usize = 4;

/// The ceiling on bytes held for a frame that has not terminated.
///
/// # Why a ceiling at all
///
/// Without one, a peer that sends bytes and never a blank line makes this buffer grow
/// until the process dies. That is not hypothetical for this client: the thing on the
/// other end of the stream is a *proxy* in front of the daemon, and a proxy that answers
/// a stream request with a non-SSE body — an HTML error page, a redirect chain, a chunked
/// response it keeps writing to — produces exactly that shape. There is no frame boundary
/// coming, so no other guard in the parser ever fires.
///
/// # Why 4 MiB
///
/// Sized from the daemon's actual frame sizes, with a large multiple of headroom.
///
/// One `output` event carries one read from a child's pipe, and the daemon reads into a
/// 16 KiB scratch buffer (`Capped::new` in `agentd/src/exec.rs`). Base64 inflates that by
/// 4/3, so the biggest `data:` line the daemon emits is about 22 KiB plus a short JSON
/// envelope — call it 24 KiB. A reattach that replays the 1 MiB stream ring does not
/// change the number: the ring is replayed as its individual chunks, each its own frame,
/// each still one 16 KiB read. `gap` and `exit` frames are tens of bytes.
///
/// 4 MiB is roughly 170 frames' worth of that, which leaves room for a proxy that
/// coalesces reads, for a future daemon with a larger scratch buffer, and for the
/// `stream_buffer_bytes` ring being raised to a few MiB — while staying small enough to be
/// irrelevant against the 512 MiB baseline VM this client talks to. A caller that hits it
/// is not reading a big frame; it is reading something that is not SSE.
///
/// The bound is on the *residue*: [`SseParser::feed`] drains every complete frame first, so
/// a single read that legitimately carries many megabytes of finished frames is not
/// refused. What is refused is a residue that keeps growing across feeds.
pub const MAX_PENDING_BYTES: usize = 4 * 1024 * 1024;

/// Feed bytes, get whole frames out. Holds a partial frame across feeds.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    /// How much of `buffer` the terminator scan has already ruled out.
    ///
    /// The scan resumes here rather than at byte zero. Rescanning from zero on every feed
    /// is quadratic in the size of a partial frame, and the buffer is largest exactly when
    /// the stream is busiest: 800 unterminated 8 KiB reads took 31 seconds to scan and now
    /// take milliseconds.
    scanned: usize,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every frame `chunk` completed. Never returns a partial one.
    ///
    /// Fails with [`ErrorKind::Protocol`] once the undelimited residue passes
    /// [`MAX_PENDING_BYTES`] — see that constant for why, and for why the check is on what
    /// is left after draining rather than on what arrived.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Frame>, Error> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        loop {
            // Back up by one byte less than the longest terminator, because a terminator
            // straddling a chunk boundary has its first bytes inside the already-scanned
            // region. Backing up to zero instead is what made this quadratic.
            let from = self.scanned.saturating_sub(MAX_TERMINATOR_LEN - 1);
            let Some((body, consumed)) = find_frame_end(&self.buffer, from) else {
                self.scanned = self.buffer.len();
                break;
            };
            let raw = self.buffer[..body].to_vec();
            self.buffer.drain(..consumed);
            // Nothing past the terminator has been looked at yet — the search stopped at
            // the first match — so the next pass starts over on what is left.
            self.scanned = 0;
            if let Some(frame) = parse_frame(&raw) {
                frames.push(frame);
            }
        }

        if self.buffer.len() > MAX_PENDING_BYTES {
            let held = self.buffer.len();
            // Dropped rather than kept: the stream is over, and holding megabytes that no
            // longer have a reader is the second half of the same bug.
            self.buffer = Vec::new();
            self.scanned = 0;
            return Err(Error::new(
                ErrorKind::Protocol,
                format!(
                    "the event stream sent {held} bytes with no frame terminator, over the \
                     {MAX_PENDING_BYTES}-byte ceiling on undelimited data. The daemon's largest \
                     frame is one 16 KiB pipe read base64-encoded, about 24 KiB, so a residue \
                     this size means the response body is not server-sent events — most often \
                     a proxy answering with an error page instead of the stream."
                ),
            ));
        }
        Ok(frames)
    }

    /// Bytes held for an incomplete frame. Diagnostic; a healthy stream sits at 0.
    pub fn pending(&self) -> usize {
        self.buffer.len()
    }
}

/// Locates the first frame terminator at or after `from`, returning the body length and
/// bytes consumed.
///
/// A frame ends at a blank line, and the wire spelling of that varies: `\n\n`,
/// `\r\n\r\n`, `\r\r`. All three are accepted because a proxy in the path may rewrite
/// line endings, and picking one spelling would work right up until it did not.
fn find_frame_end(buffer: &[u8], from: usize) -> Option<(usize, usize)> {
    let tail = buffer.get(from..)?;
    let mut earliest: Option<(usize, usize)> = None;
    for terminator in [
        b"\r\n\r\n".as_slice(),
        b"\n\n".as_slice(),
        b"\r\r".as_slice(),
    ] {
        let Some(idx) = find(tail, terminator) else {
            continue;
        };
        let idx = from + idx;
        // A shorter terminator can be found at a lower index inside a longer one
        // (`\n\n` sits at index+1 of `\r\n\r\n`), so the earliest start wins and ties
        // break toward the longer match.
        if earliest.is_none_or(|(start, _)| idx < start) {
            earliest = Some((idx, terminator.len()));
        }
    }
    let (start, width) = earliest?;
    Some((start, start + width))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Turns one frame's lines into a [`Frame`], or `None` if it carries no data.
///
/// `None` for a keepalive comment and for a frame with no `data:` line. A frame whose
/// data is not JSON is *not* dropped here — that judgement belongs to [`decode`], which
/// knows which event names carry JSON.
fn parse_frame(raw: &[u8]) -> Option<Frame> {
    let mut event = String::new();
    let mut data_lines: Vec<String> = Vec::new();
    let normalized: Vec<u8> = {
        // `\r\n` and a bare `\r` both become `\n`, so one split handles every spelling.
        let mut out = Vec::with_capacity(raw.len());
        let mut iter = raw.iter().copied().peekable();
        while let Some(byte) = iter.next() {
            if byte == b'\r' {
                if iter.peek() == Some(&b'\n') {
                    iter.next();
                }
                out.push(b'\n');
            } else {
                out.push(byte);
            }
        }
        out
    };

    for line in normalized.split(|byte| *byte == b'\n') {
        // A leading colon is a comment. axum's keepalive is exactly this.
        if line.is_empty() || line.starts_with(b":") {
            continue;
        }
        let (name, value) = match find(line, b":") {
            Some(idx) => (&line[..idx], &line[idx + 1..]),
            None => (line, b"".as_slice()),
        };
        // "If value starts with a single space, remove it" — one space only, since a
        // payload may legitimately begin with whitespace.
        let value = value.strip_prefix(b" ").unwrap_or(value);
        match String::from_utf8_lossy(name).as_ref() {
            "event" => event = String::from_utf8_lossy(value).into_owned(),
            "data" => data_lines.push(String::from_utf8_lossy(value).into_owned()),
            _ => {}
        }
    }

    if data_lines.is_empty() {
        return None;
    }
    Some(Frame {
        event,
        data: data_lines.join("\n"),
    })
}

/// One decoded stream event.
///
/// The payloads are the `protocol` crate's, so a field renamed on the daemon side
/// breaks this build. The one thing added on top is the decoded bytes of an output
/// event: base64 is how arbitrary bytes cross a JSON string, and a caller wants the
/// bytes.
///
/// Not `Clone`, because [`protocol::exec::ExitEvent`] is not — and adding the derive
/// there would be an edit to a crate this lane does not own. No consumer needs it: a
/// stream item is moved to its handler once.
#[derive(Debug)]
pub enum ExecEvent {
    /// Output bytes, with the offset they start at.
    Output {
        stream: protocol::exec::StreamKind,
        offset: u64,
        data: Vec<u8>,
    },
    /// A byte range that is gone for good.
    ///
    /// Surfaced as a typed event rather than logged, because the alternative is reading
    /// a truncated log as a complete one. `from` is inclusive and `to` exclusive, so
    /// `to` is where a cursor resumes.
    Gap { from: u64, to: u64 },
    /// The terminal event. Its absence is what distinguishes a cut connection from a
    /// finished command — the byte sequences are otherwise identical.
    Exit(protocol::exec::ExitEvent),
}

impl ExecEvent {
    /// The offset one past this event's last byte, i.e. where a cursor moves to.
    ///
    /// `None` for [`ExecEvent::Exit`]: the terminal event's offset is a total rather
    /// than a position to resume at, and treating it as a cursor would ask the daemon
    /// to replay from the end of a finished stream.
    pub fn end(&self) -> Option<u64> {
        match self {
            ExecEvent::Output { offset, data, .. } => Some(offset + data.len() as u64),
            ExecEvent::Gap { to, .. } => Some(*to),
            ExecEvent::Exit(_) => None,
        }
    }
}

/// Maps one frame onto a typed event.
///
/// Three outcomes, and the middle one is the reason this returns a nested `Result`:
///
/// * `Ok(Some(event))` — a recognized event.
/// * `Ok(None)` — a frame this client does not dispatch. An unknown event name, or a
///   payload that will not parse. Dropped rather than raised, matching the daemon,
///   which degrades a serialization failure to `{}` on purpose instead of taking the
///   connection down: one bad frame must not end an otherwise live stream.
/// * `Err(_)` — an `output` event whose base64 will not decode. Distinct from the
///   above because it is unambiguously corruption of bytes a caller asked for, and
///   silently dropping output is the failure this whole protocol is shaped to prevent.
pub fn decode(frame: &Frame) -> Result<Option<ExecEvent>, Error> {
    match frame.event.as_str() {
        protocol::exec::EVENT_OUTPUT => {
            let Ok(payload) = serde_json::from_str::<protocol::exec::OutputEvent>(&frame.data)
            else {
                return Ok(None);
            };
            let data = base64::engine::general_purpose::STANDARD
                .decode(&payload.output)
                .map_err(|err| {
                    Error::wire(
                        WireKind::ProtocolError,
                        format!(
                            "an output event at offset {} carried undecodable base64, so \
                             those bytes cannot be delivered: {err}",
                            payload.offset
                        ),
                    )
                })?;
            Ok(Some(ExecEvent::Output {
                stream: payload.stream,
                offset: payload.offset,
                data,
            }))
        }
        protocol::exec::EVENT_GAP => {
            let Ok(payload) = serde_json::from_str::<protocol::exec::GapEvent>(&frame.data) else {
                return Ok(None);
            };
            Ok(Some(ExecEvent::Gap {
                from: payload.from,
                to: payload.to,
            }))
        }
        protocol::exec::EVENT_EXIT => {
            let Ok(payload) = serde_json::from_str::<protocol::exec::ExitEvent>(&frame.data) else {
                return Ok(None);
            };
            Ok(Some(ExecEvent::Exit(payload)))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output_frame(offset: u64, bytes: &[u8]) -> String {
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        format!(
            "event: output\ndata: {{\"offset\":{offset},\"stream\":\"stdout\",\
             \"output\":\"{encoded}\"}}\n\n"
        )
    }

    /// The property the whole parser exists for: a frame split across reads survives.
    ///
    /// Byte-by-byte is the strongest version of the split — every possible boundary is
    /// exercised at once, including inside the `event:` name and inside the base64.
    #[test]
    fn a_frame_split_across_every_possible_read_boundary_still_parses() {
        let wire = output_frame(0, b"hello");
        let mut parser = SseParser::new();
        let mut frames = Vec::new();
        for byte in wire.as_bytes() {
            frames.extend(parser.feed(&[*byte]).expect("under the pending ceiling"));
        }
        assert_eq!(frames.len(), 1, "the split frame did not reassemble");
        assert_eq!(parser.pending(), 0, "a complete stream left bytes behind");

        let event = decode(&frames[0]).expect("decodes").expect("is an event");
        match event {
            ExecEvent::Output { offset, data, .. } => {
                assert_eq!(offset, 0);
                assert_eq!(data, b"hello");
            }
            other => panic!("expected output, got {other:?}"),
        }
    }

    /// Several frames in one read all come out, in order.
    #[test]
    fn one_read_carrying_three_frames_yields_three() {
        let wire = format!(
            "{}{}{}",
            output_frame(0, b"a"),
            output_frame(1, b"b"),
            "event: exit\ndata: {\"exit_code\":0,\"signal\":null,\"truncated\":false,\
             \"writers_may_be_alive\":false,\"offset\":2}\n\n"
        );
        let mut parser = SseParser::new();
        let frames = parser
            .feed(wire.as_bytes())
            .expect("under the pending ceiling");
        assert_eq!(frames.len(), 3);
        assert!(matches!(
            decode(&frames[2]).expect("decodes"),
            Some(ExecEvent::Exit(_))
        ));
    }

    /// A trailing partial frame is held, not emitted and not lost.
    #[test]
    fn a_trailing_partial_frame_is_held_until_it_completes() {
        let wire = output_frame(0, b"xyz");
        let (head, tail) = wire.split_at(wire.len() - 4);
        let mut parser = SseParser::new();
        assert!(
            parser
                .feed(head.as_bytes())
                .expect("under the pending ceiling")
                .is_empty(),
            "an incomplete frame was emitted"
        );
        assert!(parser.pending() > 0);
        let frames = parser
            .feed(tail.as_bytes())
            .expect("under the pending ceiling");
        assert_eq!(frames.len(), 1);
    }

    /// The scan cursor does not skip a terminator that straddles a chunk boundary.
    ///
    /// This is the way a resumed scan gets it wrong: the previous feed ended part-way
    /// through the blank line, so the terminator's first bytes are inside the region
    /// already ruled out. Every split of every spelling is exercised, which is what makes
    /// the back-up-by-terminator-length-minus-one arithmetic pinned rather than sampled.
    ///
    /// **Falsification** — resume the scan at `self.scanned` with no back-up and the
    /// `\r\n\r\n` and `\r\r` splits stop producing a frame at all.
    #[test]
    fn a_terminator_straddling_a_chunk_boundary_is_still_found() {
        for terminator in ["\n\n", "\r\n\r\n", "\r\r"] {
            let wire = format!("event: gap\ndata: {{\"from\":0,\"to\":4}}{terminator}");
            for split in 1..wire.len() {
                let mut parser = SseParser::new();
                let mut frames = Vec::new();
                frames.extend(
                    parser
                        .feed(&wire.as_bytes()[..split])
                        .expect("under the pending ceiling"),
                );
                frames.extend(
                    parser
                        .feed(&wire.as_bytes()[split..])
                        .expect("under the pending ceiling"),
                );
                assert_eq!(
                    frames.len(),
                    1,
                    "{terminator:?} split at {split} did not reassemble"
                );
                assert_eq!(
                    parser.pending(),
                    0,
                    "{terminator:?} split at {split} left bytes behind"
                );
                assert!(matches!(
                    decode(&frames[0]).expect("decodes"),
                    Some(ExecEvent::Gap { from: 0, to: 4 })
                ));
            }
        }
    }

    /// An undelimited stream is refused at the ceiling rather than buffered forever.
    ///
    /// [`ErrorKind::Protocol`] and therefore **not retryable**, which is the half that
    /// matters to `exec::Attach`: a reconnect into the same non-SSE body would refill the
    /// buffer `max_reconnects` times over.
    ///
    /// **Falsification** — drop the ceiling check and the parser accepts every chunk, so
    /// `expect_err` panics.
    #[test]
    fn an_unterminated_stream_is_refused_at_the_pending_ceiling() {
        let mut parser = SseParser::new();
        // 8 KiB with no blank line anywhere, which is what a proxy's HTML error page or a
        // redirect chain looks like to this parser.
        let chunk = vec![b'x'; 8 * 1024];
        let mut fed = 0usize;
        loop {
            match parser.feed(&chunk) {
                Ok(frames) => {
                    assert!(frames.is_empty(), "undelimited data produced a frame");
                    fed += chunk.len();
                    assert!(
                        parser.pending() <= MAX_PENDING_BYTES,
                        "the buffer passed the ceiling without erroring: {}",
                        parser.pending()
                    );
                    assert!(
                        fed <= MAX_PENDING_BYTES + chunk.len(),
                        "the ceiling never fired"
                    );
                }
                Err(err) => {
                    assert_eq!(err.kind(), ErrorKind::Protocol);
                    assert!(
                        !err.retryable(),
                        "a reconnect would refill the buffer from the same body"
                    );
                    assert_eq!(
                        err.wire_kind(),
                        None,
                        "no status produced this; it is a client-side ceiling"
                    );
                    let message = err.to_string();
                    assert!(
                        message.contains(&MAX_PENDING_BYTES.to_string()),
                        "the message must name the ceiling: {message}"
                    );
                    assert!(
                        message.contains("no frame terminator"),
                        "the message must say what was wrong: {message}"
                    );
                    assert_eq!(
                        parser.pending(),
                        0,
                        "the refused bytes are dropped, not held for a reader that is gone"
                    );
                    return;
                }
            }
        }
    }

    /// Scanning is linear in the buffer, not quadratic.
    ///
    /// The measured shape of the defect: 800 unterminated 8 KiB reads — 6.4 MiB, one read
    /// under the ceiling per feed — rescanned from byte zero every time.
    ///
    /// **The budget is 10 s for a workload that takes ~0.75 s fixed and ~160 s broken.**
    /// Deliberately nowhere near the fixed figure, because the two populations are 200x
    /// apart and the only way to lose that discrimination is to make the threshold tight
    /// enough for load to cross it. It was 500 ms and did exactly that: measured
    /// 2026-08-09 on a 16-core box at load average 26, the fixed path took 706-847 ms
    /// across nine runs and this assertion failed every time — a red gate over code with
    /// no defect in it, which is the failure mode that teaches people to skip a gate.
    /// A 13x margin over the fixed path still leaves a 16x margin under the broken one.
    ///
    /// **Falsification** — replace `from` with `0` in `feed` and this takes 160 s
    /// (verified 2026-08-09), so the guard fails by two orders of magnitude rather than
    /// by a hair.
    #[test]
    fn scanning_an_unterminated_buffer_stays_linear() {
        let chunk = vec![b'x'; 8 * 1024];
        let started = std::time::Instant::now();
        let mut parser = SseParser::new();
        for _ in 0..800 {
            // A feed that trips the ceiling resets the buffer, which would hide the
            // quadratic scan — so the parser is rebuilt instead of being allowed to grow
            // past it, and each iteration still scans a buffer the old code rescanned
            // whole.
            if parser.pending() + chunk.len() > MAX_PENDING_BYTES {
                parser = SseParser::new();
            }
            parser.feed(&chunk).expect("under the pending ceiling");
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "800 x 8 KiB unterminated feeds took {elapsed:?}, over the 10s budget; the \
             quadratic rescan is back (the linear path is ~0.75s even under heavy load, \
             and the quadratic one is ~160s)"
        );
    }

    /// All three blank-line spellings terminate a frame, because a proxy may rewrite
    /// line endings and picking one spelling works right up until it does not.
    #[test]
    fn every_blank_line_spelling_terminates_a_frame() {
        for terminator in ["\n\n", "\r\n\r\n", "\r\r"] {
            let wire = format!("event: gap\r\ndata: {{\"from\":0,\"to\":4}}{terminator}");
            let mut parser = SseParser::new();
            let frames = parser
                .feed(wire.as_bytes())
                .expect("under the pending ceiling");
            assert_eq!(frames.len(), 1, "{terminator:?} did not terminate a frame");
            assert!(matches!(
                decode(&frames[0]).expect("decodes"),
                Some(ExecEvent::Gap { from: 0, to: 4 })
            ));
        }
    }

    /// A keepalive is a comment with no data, and it must not surface as an event —
    /// otherwise fifteen seconds of silence looks like output.
    #[test]
    fn a_keepalive_comment_is_not_a_frame() {
        let mut parser = SseParser::new();
        assert!(
            parser
                .feed(b":\n\n")
                .expect("under the pending ceiling")
                .is_empty()
        );
        assert!(
            parser
                .feed(b": keep-alive-text\n\n")
                .expect("under the pending ceiling")
                .is_empty()
        );
        assert_eq!(parser.pending(), 0);

        // And it does not disturb a real frame that follows it on the same read.
        let wire = format!(":\n\n{}", output_frame(0, b"real"));
        let mut parser = SseParser::new();
        assert_eq!(
            parser
                .feed(wire.as_bytes())
                .expect("under the pending ceiling")
                .len(),
            1
        );
    }

    /// One space after the colon is stripped, a second is payload.
    ///
    /// The daemon writes `data: {...}`, and a parser that stripped all leading
    /// whitespace would corrupt any payload that meant to start with a space.
    #[test]
    fn exactly_one_leading_space_is_stripped_from_a_field_value() {
        let mut parser = SseParser::new();
        let frames = parser
            .feed(b"event:  gap\ndata:  {\"from\":1,\"to\":2}\n\n")
            .expect("under the pending ceiling");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, " gap", "more than one space was stripped");
        assert_eq!(frames[0].data, " {\"from\":1,\"to\":2}");
        // serde_json tolerates the leading space, so the event still decodes — but the
        // event *name* no longer matches, which is the failure this would cause.
        assert!(
            decode(&frames[0]).expect("decodes").is_none(),
            "a mis-stripped name must not resolve to a known event"
        );
    }

    /// An unknown event name and unparseable data are both dropped, matching the
    /// daemon's own degradation. One bad frame must not end a live stream.
    #[test]
    fn an_unrecognized_or_unparseable_frame_is_dropped_rather_than_raised() {
        let mut parser = SseParser::new();
        let frames = parser
            .feed(
                b"event: something-new\ndata: {}\n\n\
              event: gap\ndata: not json at all\n\n",
            )
            .expect("under the pending ceiling");
        assert_eq!(frames.len(), 2);
        for frame in &frames {
            assert!(
                decode(frame).expect("no error").is_none(),
                "{frame:?} should be dropped"
            );
        }
    }

    /// Undecodable base64 in an *output* event is an error, not a drop.
    ///
    /// The one place a drop would be wrong: those are bytes the caller asked for, and
    /// silently losing output is the failure the gap event exists to make impossible.
    #[test]
    fn undecodable_output_base64_is_an_error_rather_than_a_silent_drop() {
        let mut parser = SseParser::new();
        let frames = parser.feed(
            b"event: output\ndata: {\"offset\":7,\"stream\":\"stdout\",\"output\":\"!!!!\"}\n\n",
        ).expect("under the pending ceiling");
        let err = decode(&frames[0]).expect_err("bad base64 must be reported");
        assert!(err.to_string().contains('7'), "{err}");
    }

    /// A cursor moves past delivered bytes and past a gap, and never past an exit.
    #[test]
    fn an_events_end_is_where_a_cursor_resumes() {
        let output = ExecEvent::Output {
            stream: protocol::exec::StreamKind::Stdout,
            offset: 10,
            data: b"abcd".to_vec(),
        };
        assert_eq!(output.end(), Some(14));
        assert_eq!(ExecEvent::Gap { from: 3, to: 9 }.end(), Some(9));
        assert_eq!(
            ExecEvent::Exit(protocol::exec::ExitEvent {
                exit_code: Some(0),
                signal: None,
                truncated: false,
                writers_may_be_alive: false,
                offset: 99,
            })
            .end(),
            None,
            "the terminal offset is a total, not a resume point"
        );
    }

    /// A multi-line `data:` payload joins with newlines, as the spec says.
    #[test]
    fn a_multi_line_data_payload_joins_with_newlines() {
        let mut parser = SseParser::new();
        let frames = parser
            .feed(b"event: gap\ndata: {\"from\":0,\ndata: \"to\":5}\n\n")
            .expect("under the pending ceiling");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "{\"from\":0,\n\"to\":5}");
        assert!(matches!(
            decode(&frames[0]).expect("decodes"),
            Some(ExecEvent::Gap { from: 0, to: 5 })
        ));
    }
}
