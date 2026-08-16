# SPDX-License-Identifier: Apache-2.0
"""Shared fixtures for the unit tiers.

The one thing worth sharing is an **offline SSE server**. Everything else in this suite is a
pure call into the extension module, but the exec/stream surface is the one place where a
meaningful assertion needs something to talk to — and the daemon is not available in a unit
run. `sse_server` is that something: a `http.server` on loopback that answers a
`GET /v1/exec/<id>/stream` with a scripted list of SSE frames.

# What this does and does not cover

It covers the parts of the stream contract that are *this client's* behaviour rather than the
daemon's: the frames it parses, the events it hands back, the cursor its reconnect asks for,
and where a `Gap` becomes an exception. Those are properties of `microvms-core`'s state machine
as reached **through the binding**, which is the thing under test here — the binding's task, its
capacity-1 channel, and its iterator.

It does not cover the daemon: nothing here starts `agentd`, so nothing here asserts that the
daemon emits these frames in the first place. That is the conformance suite's job, and the
boundary is honest rather than papered over — a script below is a transcription of the frame
shapes `microvms-core/src/session/sse.rs` parses, and if the daemon's framing changed, these
tests would keep passing while the conformance suite went red.
"""

from __future__ import annotations

import base64
import http.server
import socketserver
import threading
from collections.abc import Iterator, Sequence

import pytest


def output_frame(offset: int, data: bytes, stream: str = "stdout") -> bytes:
    """One SSE `output` frame, base64 as the wire carries it."""
    encoded = base64.b64encode(data).decode()
    return (
        f"event: output\n"
        f'data: {{"offset":{offset},"stream":"{stream}","output":"{encoded}"}}\n\n'
    ).encode()


def gap_frame(start: int, end: int) -> bytes:
    """One SSE `gap` frame: `[start, end)` is gone for good."""
    return f'event: gap\ndata: {{"from":{start},"to":{end}}}\n\n'.encode()


def exit_frame(
    total: int, exit_code: int | None = 0, signal: int | None = None
) -> bytes:
    """The terminal `exit` frame. Its absence is what makes a stream a cut."""
    code = "null" if exit_code is None else str(exit_code)
    sig = "null" if signal is None else str(signal)
    return (
        f"event: exit\n"
        f'data: {{"exit_code":{code},"signal":{sig},"truncated":false,'
        f'"writers_may_be_alive":false,"offset":{total}}}\n\n'
    ).encode()


class SseServer:
    """A loopback SSE server that answers each attach from a scripted frame list.

    `scripts` is one list of frames per attach, so a two-element `scripts` is how a **cut and
    reconnect** is expressed: the first response ends without an `exit` frame, which is exactly
    the condition the core's reconnect keys on. `requested_paths` is what makes the reconnect
    assertable — the offset a second attach asks for is in the query string, and that number is
    the whole point of the cursor.
    """

    def __init__(self, scripts: Sequence[Sequence[bytes]]) -> None:
        self.requested_paths: list[str] = []
        remaining = iter(list(scripts))
        paths = self.requested_paths

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            # `do_GET` rather than `do_get` because it is http.server's own dispatch
            # spelling — `BaseHTTPRequestHandler` looks up `"do_" + command`, so a
            # snake_case name is simply never called. This line carried a suppression of
            # N802 until ruff learned the convention itself: measured on 0.15.22, that rule
            # exempts `do_*` on a `BaseHTTPRequestHandler` subclass, so the directive
            # suppressed nothing and RUF100 flagged it. The reason survives as prose; the
            # dead suppression does not.
            def do_GET(self) -> None:
                paths.append(self.path)
                try:
                    frames = next(remaining)
                except StopIteration:
                    # A script that ran out answers an empty body, which the core reads as a
                    # cut. Better than hanging: a test that over-attaches should fail on its
                    # own assertion rather than on a timeout.
                    frames = []
                body = b"".join(frames)
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                self.wfile.flush()

            def log_message(self, *args: object) -> None:
                """Silent: a passing test should print nothing."""

        self._server = socketserver.ThreadingTCPServer(("127.0.0.1", 0), Handler)
        self._server.daemon_threads = True
        self.port = self._server.server_address[1]
        # A 10ms poll rather than `serve_forever`'s 0.5s default, which is a *teardown* cost:
        # `shutdown()` waits for the loop to notice, so the default put half a second on every
        # test in this file — measured at 11.7s for the module before this argument.
        self._thread = threading.Thread(
            target=self._server.serve_forever, args=(0.01,), daemon=True
        )
        self._thread.start()

    @property
    def endpoint(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def offsets_requested(self) -> list[int]:
        """The `?offset=` each attach asked for, in order."""
        return [int(path.split("offset=")[1]) for path in self.requested_paths]

    def close(self) -> None:
        self._server.shutdown()
        self._server.server_close()


@pytest.fixture
def sse_server() -> Iterator[type[SseServer]]:
    """Hands the class back, so a test scripts its own frames and still gets teardown.

    A factory rather than a ready-made server because every test wants a different script, and
    the teardown is what matters: a leaked thread makes the *next* test flaky, which is the
    worst failure mode a fixture can have.
    """
    built: list[SseServer] = []

    def factory(scripts: Sequence[Sequence[bytes]]) -> SseServer:
        server = SseServer(scripts)
        built.append(server)
        return server

    yield factory  # type: ignore[misc]
    for server in built:
        server.close()
