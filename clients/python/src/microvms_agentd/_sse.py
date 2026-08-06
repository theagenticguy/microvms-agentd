"""Incremental Server-Sent Events parser.

Incremental because the transport decides where the reads land, not the sender: a
single `data:` line routinely arrives split across two chunks, and a parser that
assumes a read boundary is a frame boundary loses output at exactly the moment the
stream gets busy. So bytes are buffered until a blank line proves a frame is
complete, and a trailing partial frame stays in the buffer.

Only the subset the daemon emits is implemented: named events with one `data:` line
of JSON, plus the `:` keepalive comments axum sends every 15 seconds. `id:` and
`retry:` are ignored rather than half-supported — the daemon does not send them,
and the byte offset is our resume cursor instead of `Last-Event-ID`.
"""

from __future__ import annotations

import json
from collections.abc import Iterator
from dataclasses import dataclass
from typing import Any


@dataclass(frozen=True)
class Frame:
    """One complete SSE event: its name and its decoded JSON payload."""

    event: str
    data: Any


class SseParser:
    """Feed bytes, get whole frames out. Holds a partial frame across feeds."""

    def __init__(self) -> None:
        self._buffer = bytearray()

    def feed(self, chunk: bytes) -> Iterator[Frame]:
        """Yields every frame that `chunk` completed. Never yields a partial one."""
        self._buffer.extend(chunk)
        while True:
            end = _find_frame_end(self._buffer)
            if end is None:
                return
            raw, consumed = end
            del self._buffer[:consumed]
            frame = _parse_frame(bytes(raw))
            if frame is not None:
                yield frame

    @property
    def pending(self) -> int:
        """Bytes held for an incomplete frame. Diagnostic; a healthy stream sits at 0."""
        return len(self._buffer)


def _find_frame_end(buffer: bytearray) -> tuple[bytearray, int] | None:
    """Locates the first frame terminator, returning the frame body and bytes consumed.

    A frame ends at a blank line, and the wire spelling of that varies: `\\n\\n`,
    `\\r\\n\\r\\n`, `\\r\\r`. All three are accepted because a proxy in the path may
    rewrite line endings, and picking one spelling would work right up until it did
    not.
    """
    earliest: tuple[int, int] | None = None
    for terminator in (b"\r\n\r\n", b"\n\n", b"\r\r"):
        idx = buffer.find(terminator)
        if idx == -1:
            continue
        # A shorter terminator can be found at a lower index inside a longer one
        # (`\n\n` sits at index+1 of `\r\n\r\n`), so the earliest start wins and
        # ties break toward the longer match.
        if earliest is None or idx < earliest[0]:
            earliest = (idx, len(terminator))
    if earliest is None:
        return None
    start, width = earliest
    return buffer[:start], start + width


def _parse_frame(raw: bytes) -> Frame | None:
    """Turns one frame's lines into a `Frame`, or `None` if it carries no data.

    Returns `None` for a keepalive comment and for any frame whose `data:` is not
    JSON. Malformed JSON is dropped rather than raised: the daemon degrades a
    serialization failure to `{}` on purpose instead of taking the connection down,
    and matching that here keeps one bad frame from ending an otherwise live stream.
    """
    event = ""
    data_lines: list[str] = []
    for line in raw.replace(b"\r\n", b"\n").replace(b"\r", b"\n").split(b"\n"):
        if not line or line.startswith(b":"):
            # A leading colon is a comment. axum's keepalive is exactly this.
            continue
        name, _, value = line.partition(b":")
        # "If value starts with a single space, remove it" — one space only, since
        # a payload may legitimately begin with whitespace.
        if value.startswith(b" "):
            value = value[1:]
        field = name.decode("utf-8", "replace")
        if field == "event":
            event = value.decode("utf-8", "replace")
        elif field == "data":
            data_lines.append(value.decode("utf-8", "replace"))

    if not data_lines:
        return None
    try:
        payload = json.loads("\n".join(data_lines))
    except json.JSONDecodeError:
        return None
    return Frame(event=event, data=payload)
