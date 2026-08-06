"""A local stand-in for the daemon, so every test runs with no AWS and no VM.

A real socket rather than a mocked httpx transport, because two of the properties
under test are transport properties: a stream that is *cut mid-body* has to be
distinguishable from one that ended, and an SSE frame has to survive arriving in
two reads. A mock that hands back whole responses cannot express either.
"""

from __future__ import annotations

import base64
import json
import threading
from collections.abc import Callable
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.parse import parse_qs, urlparse


@dataclass
class Recorded:
    method: str
    path: str
    query: dict[str, list[str]]
    headers: dict[str, str]
    body: bytes


@dataclass
class Route:
    """One canned reply, or a handler that writes the body itself.

    `writer` exists for the streaming cases: it is handed the raw wfile so a test
    can flush a partial frame, sleep, then flush the rest — or close the socket
    mid-body to simulate the drop that `stream()` must recover from.
    """

    status: int = 200
    body: bytes = b""
    content_type: str = "application/json"
    writer: Callable[[FakeDaemon, Any], None] | None = None


class FakeDaemon:
    """A threaded HTTP server whose routes a test installs by `(method, path)`."""

    def __init__(self) -> None:
        self.routes: dict[tuple[str, str], list[Route]] = {}
        self.requests: list[Recorded] = []
        self._server: ThreadingHTTPServer | None = None
        self._thread: threading.Thread | None = None
        self.state: dict[str, Any] = {}

    def on(self, method: str, path: str, *replies: Route) -> None:
        """Installs replies for a route.

        Multiple replies are consumed in order, and the last one repeats. That is
        what makes "fail once, then succeed" expressible, which is the shape of
        every retry test here.
        """
        self.routes[(method.upper(), path)] = list(replies)

    def take(self, method: str, path: str) -> Route:
        queue = self.routes.get((method.upper(), path))
        if not queue:
            return Route(status=404, body=b'{"error":"no route installed"}')
        return queue.pop(0) if len(queue) > 1 else queue[0]

    def calls(self, method: str, path: str) -> list[Recorded]:
        return [r for r in self.requests if r.method == method.upper() and r.path == path]

    @property
    def url(self) -> str:
        assert self._server is not None
        port = self._server.server_address[1]
        return f"http://127.0.0.1:{port}"

    def start(self) -> None:
        daemon = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_: Any) -> None:
                pass

            def _handle(self, method: str) -> None:
                parsed = urlparse(self.path)
                length = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(length) if length else b""
                daemon.requests.append(
                    Recorded(
                        method=method,
                        path=parsed.path,
                        query=parse_qs(parsed.query),
                        headers={k.lower(): v for k, v in self.headers.items()},
                        body=body,
                    )
                )
                route = daemon.take(method, parsed.path)
                if route.writer is not None:
                    route.writer(daemon, self)
                    return
                self.send_response(route.status)
                self.send_header("Content-Type", route.content_type)
                self.send_header("Content-Length", str(len(route.body)))
                self.end_headers()
                if route.body:
                    self.wfile.write(route.body)

            def do_GET(self) -> None:
                self._handle("GET")

            def do_POST(self) -> None:
                self._handle("POST")

            def do_PUT(self) -> None:
                self._handle("PUT")

        class Server(ThreadingHTTPServer):
            def handle_error(self, *_: Any) -> None:
                # Silenced, not ignored. Half of these tests deliberately close a
                # socket mid-body, and the stdlib prints a full traceback to stderr
                # for each one — noise that reads like a failure in an otherwise
                # green run. The client side is where those drops get asserted on.
                pass

        self._server = Server(("127.0.0.1", 0), Handler)
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        if self._server is not None:
            self._server.shutdown()
            self._server.server_close()
        if self._thread is not None:
            self._thread.join(timeout=5)


def sse(event: str, payload: dict[str, Any]) -> bytes:
    """One SSE frame as the daemon writes it."""
    return f"event: {event}\ndata: {json.dumps(payload)}\n\n".encode()


def output_frame(offset: int, text: str, stream: str = "stdout") -> bytes:
    return sse(
        "output",
        {
            "offset": offset,
            "stream": stream,
            "output": base64.b64encode(text.encode()).decode(),
        },
    )


def exit_frame(offset: int, code: int = 0) -> bytes:
    return sse(
        "exit",
        {
            "exit_code": code,
            "signal": None,
            "truncated": False,
            "writers_may_be_alive": False,
            "offset": offset,
        },
    )


def chunked_writer(chunks: list[bytes], *, cut: bool = False) -> Callable[[FakeDaemon, Any], None]:
    """Writes an SSE body one chunk at a time, optionally hanging up mid-body.

    `cut=True` closes the connection without a terminating zero-length chunk, which
    is what a dropped stream looks like on the wire — and the condition `stream()`
    has to recover from by reconnecting at its last good offset.
    """

    def write(_: FakeDaemon, handler: Any) -> None:
        handler.send_response(200)
        handler.send_header("Content-Type", "text/event-stream")
        handler.send_header("Transfer-Encoding", "chunked")
        handler.end_headers()
        for chunk in chunks:
            handler.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
            handler.wfile.flush()
        if cut:
            # No terminating chunk: the body ends because the socket does.
            handler.close_connection = True
            try:
                handler.wfile.flush()
                handler.connection.close()
            except OSError:
                pass
            return
        handler.wfile.write(b"0\r\n\r\n")
        handler.wfile.flush()

    return write


@dataclass
class FakeMicrovmClient:
    """The one boto3 method `ProxyAuth` calls, plus a failure switch."""

    tokens: list[str] = field(default_factory=lambda: ["token-0", "token-1", "token-2"])
    calls: list[dict[str, Any]] = field(default_factory=list)
    fail_times: int = 0

    def create_microvm_auth_token(self, **kwargs: Any) -> dict[str, Any]:
        self.calls.append(kwargs)
        if self.fail_times > 0:
            self.fail_times -= 1
            raise RuntimeError("ThrottlingException: Rate exceeded")
        value = self.tokens[min(len(self.calls) - 1, len(self.tokens) - 1)]
        # A map of header name to value, not a bare string. Shaped exactly as the
        # real API answers, because a client that assumed a string is the defect
        # this shape exists to catch.
        return {"authToken": {"X-aws-proxy-auth": value}}
