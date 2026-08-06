"""Typed errors, one per status the daemon actually chooses.

The daemon picks each status deliberately and `docs/PROTOCOL.md` names the defect
that bought it, so the mapping here is a contract rather than a convenience. The
split that matters to a caller is retryable vs fatal: `NotBootstrapped` means
"come back in a moment", `Unauthorized` means "your credential is wrong and no
amount of waiting fixes it". Getting that wrong in either direction is expensive —
retrying a 401 forever, or failing a launch that was 200ms from being ready.

Callers write `except NotBootstrapped:` and never parse a message.
"""

from __future__ import annotations


class AgentdError(Exception):
    """Base for everything this library raises."""

    #: Whether retrying the identical request could plausibly succeed.
    retryable: bool = False


class TransportError(AgentdError):
    """The request never produced a status: connection refused, reset, timeout.

    Retryable because it says nothing about the daemon's state. A VM that has
    just reached RUNNING commonly refuses a connection or two before the proxy
    path is wired up.
    """

    retryable = True


class AuthTokenMintError(AgentdError):
    """`CreateMicrovmAuthToken` failed.

    Retryable, and this is load-bearing rather than optimistic: proxy tokens
    expire at 60 minutes, so a long run *will* mint mid-flight, and a throttle
    from the control plane at that moment must not kill a trial that is otherwise
    healthy. See `docs/PLATFORM.md`, "Endpoint authentication".
    """

    retryable = True


class HttpError(AgentdError):
    """A response arrived with a status the daemon uses to mean something."""

    status: int = 0

    def __init__(self, message: str, *, status: int | None = None, body: bytes = b"") -> None:
        super().__init__(message)
        if status is not None:
            self.status = status
        self.body = body


class NotBootstrapped(HttpError):
    """503: the run hook has not landed, so the control API is closed.

    Not 404 and not a dropped connection — the daemon is explicit about this
    because a client that reads it as "route missing" concludes the daemon is the
    wrong version. Retry: the platform is about to deliver the token.
    """

    status = 503
    retryable = True


class Unauthorized(HttpError):
    """401: the presented bearer token is not the installed one. Fatal."""

    status = 401


class ProtocolError(HttpError):
    """400: malformed body, missing query key, bad mode, refused tar member.

    Never 404. A 404 here would be read as a missing file, which is exactly how
    one defect hid for a review round.
    """

    status = 400


class NotFound(HttpError):
    """404: a genuinely absent exec id, file, or directory."""

    status = 404


class Conflict(HttpError):
    """409: the request is well-formed but the target is in the wrong state.

    A bootstrap hijack, a second ack, acking a still-running exec, or writing
    stdin to an exec started without `stdin: true`.
    """

    status = 409


class TooLarge(HttpError):
    """413: over a configured cap (body, tar members/bytes, stdin write)."""

    status = 413


class StdinClosed(HttpError):
    """410: stdin already saw EOF, or the child stopped reading. Never retryable.

    Distinct from `Conflict`, which is "you did not ask for stdin" and is fixed at
    start time. This one is a lifecycle fact.
    """

    status = 410


class RequestTimeout(HttpError):
    """408: the child did not drain its stdin pipe within the write timeout.

    Retryable, and the daemon deliberately keeps its stdin handle open so that a
    retry can succeed. Some bytes may already have been written — reconciling
    that is the caller's problem, which is why it is a distinct type.
    """

    status = 408
    retryable = True


class ServerError(HttpError):
    """5xx other than 503: spawn failure, io failure, a panicking task."""

    status = 500
    retryable = True


class ExecTimeout(AgentdError):
    """A client-side wait or stream deadline elapsed. The exec is untouched.

    Polling and attaching are read-only, so giving up here has not affected the
    command: the record and its output are still there to be re-polled.
    """


class OutputGap(AgentdError):
    """Raised for a `gap` SSE event when the caller asked to be interrupted by one.

    A gap means bytes are gone for good — the replay ring evicted them, or this
    subscriber lagged the live channel. The alternative to surfacing it is reading
    a truncated log as a complete one.
    """

    def __init__(self, start: int, end: int) -> None:
        super().__init__(f"output bytes [{start}, {end}) are unrecoverable")
        self.start = start
        self.end = end


# 400 must not be reachable through a generic 4xx fallback: the daemon's whole
# point is that 400 and 404 mean different things, and a fallback that collapsed
# them would reintroduce the phantom-missing-file defect.
_BY_STATUS: dict[int, type[HttpError]] = {
    400: ProtocolError,
    401: Unauthorized,
    404: NotFound,
    408: RequestTimeout,
    409: Conflict,
    410: StdinClosed,
    413: TooLarge,
    503: NotBootstrapped,
}


def error_for_status(status: int, body: bytes, *, method: str, path: str) -> HttpError:
    """Builds the typed error for a status, or a base `HttpError` for an unmapped one."""
    detail = body[:512].decode("utf-8", "replace").strip()
    message = f"{method} {path} -> {status}" + (f": {detail}" if detail else "")
    cls = _BY_STATUS.get(status)
    if cls is not None:
        return cls(message, status=status, body=body)
    if status >= 500:
        return ServerError(message, status=status, body=body)
    return HttpError(message, status=status, body=body)
