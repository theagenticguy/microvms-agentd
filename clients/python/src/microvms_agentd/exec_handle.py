"""The handle a caller holds for one running exec.

The whole reason this file exists is `stream()`. Everything else here is a thin
wrapper over one route.
"""

from __future__ import annotations

import base64
import time
from collections.abc import Iterator
from typing import TYPE_CHECKING

import httpx

from ._sse import SseParser
from .errors import (
    AgentdError,
    AuthTokenMintError,
    ExecTimeout,
    NotFound,
    OutputGap,
    TransportError,
)
from .models import ExecResult, Exit, Gap, OutputChunk, Phase, StdinAck, StreamEvent, StreamKind

if TYPE_CHECKING:
    from .session import Session

#: Backoff between stream reconnect attempts. Short, because the offset makes a
#: reconnect cheap — the daemon replays from the cursor, so nothing is refetched
#: that was already delivered.
_RECONNECT_BACKOFF_SEC = (0.25, 0.5, 1.0, 2.0, 4.0)

#: How long an attached stream may be silent before it is treated as dead. Four
#: times the daemon's 15s SSE keepalive, so three missed keepalives are tolerated
#: before a reconnect. Tighter than that turns a slow proxy into a reconnect loop.
DEFAULT_STREAM_IDLE_TIMEOUT_SEC = 60.0


class ExecHandle:
    """One exec, addressed by its caller-minted id.

    The id is the idempotency key, so a handle survives a process restart: rebuild
    it with the same id and every method still addresses the same server-side exec.
    """

    def __init__(
        self,
        session: Session,
        exec_id: str,
        *,
        idle_timeout: float = DEFAULT_STREAM_IDLE_TIMEOUT_SEC,
    ) -> None:
        self._session = session
        self.exec_id = exec_id
        self.idle_timeout = idle_timeout

    def __repr__(self) -> str:
        return f"ExecHandle(exec_id={self.exec_id!r})"

    def poll(self) -> ExecResult:
        """Reads current status and output. Read-only server-side; safe to spin on."""
        response = self._session.transport.send("GET", f"/v1/exec/{self.exec_id}")
        return ExecResult.from_json(response.json())

    def wait(self, timeout: float = 300.0, interval: float = 1.0) -> ExecResult:
        """Polls until the exec is done, or raises `ExecTimeout`.

        A timeout here has not touched the exec — polling is read-only and the
        output lives until it is acked — so a caller that gives up can come back
        and poll again.

        A transport error mid-wait is swallowed and retried rather than raised: a
        VM under load drops a connection occasionally, and the whole point of a
        read-only poll is that repeating it costs nothing.
        """
        deadline = time.monotonic() + timeout
        last: ExecResult | None = None
        while True:
            try:
                last = self.poll()
                if last.done:
                    return last
            except (TransportError, AuthTokenMintError):
                pass
            if time.monotonic() >= deadline:
                phase = last.phase.value if last else "unknown"
                raise ExecTimeout(
                    f"exec {self.exec_id} was still {phase} after {timeout:.0f}s; "
                    "the record and its output are untouched and can be re-polled"
                )
            time.sleep(min(interval, max(0.0, deadline - time.monotonic())))

    def stream(
        self,
        *,
        offset: int = 0,
        raise_on_gap: bool = False,
        reconnect: bool = True,
        max_reconnects: int = 20,
        timeout: float | None = None,
    ) -> Iterator[StreamEvent]:
        """Yields output as it arrives, reconnecting at the last good offset.

        Reconnect is the point of the offset, and the offset is the point of the
        route. Without it a dropped connection means either losing everything after
        the drop or replaying everything from zero — E2B's equivalent has exactly
        this defect. With it, a reconnect resumes at the byte after the last one
        delivered, and the only bytes that can go missing are ones the daemon has
        already evicted from its replay ring, which it reports as a `gap`.

        The reconnect condition is precise: a stream that ended *without* an `exit`
        event was cut, and a stream that ended *with* one is over. That is the
        distinction SSE framing buys and it is why this is not a byte stream.

        A `gap` is yielded as a typed `Gap`, never skipped. Set `raise_on_gap` to
        turn one into an `OutputGap` exception instead, which is what a caller that
        must have complete output wants.
        """
        cursor = offset
        attempts = 0
        deadline = None if timeout is None else time.monotonic() + timeout

        while True:
            saw_exit = False
            try:
                for event in self._attach(cursor, deadline):
                    if isinstance(event, OutputChunk):
                        # Advance only past bytes actually handed to the caller, so
                        # a reconnect never re-delivers and never skips.
                        cursor = event.end
                        yield event
                    elif isinstance(event, Gap):
                        # The daemon has already moved its own cursor past the gap;
                        # ours follows, otherwise a reconnect would ask for evicted
                        # bytes again and be told about the same gap forever.
                        cursor = max(cursor, event.end)
                        if raise_on_gap:
                            raise OutputGap(event.start, event.end)
                        yield event
                    else:
                        saw_exit = True
                        yield event
            except (TransportError, AuthTokenMintError):
                # A cut connection or a failed mint. Neither says anything about
                # the exec, which is still running server-side.
                if not reconnect:
                    raise
            except NotFound:
                # The entry was collected (acked, then past its TTL). Reconnecting
                # can never succeed, so this is fatal regardless of `reconnect`.
                raise

            if saw_exit:
                return
            if not reconnect:
                return
            if deadline is not None and time.monotonic() >= deadline:
                raise ExecTimeout(
                    f"stream of {self.exec_id} exceeded {timeout:.0f}s at offset {cursor}"
                )
            attempts += 1
            if attempts > max_reconnects:
                raise AgentdError(
                    f"stream of {self.exec_id} dropped {attempts} times without an exit "
                    f"event; last good offset {cursor}"
                )
            backoff = _RECONNECT_BACKOFF_SEC[min(attempts - 1, len(_RECONNECT_BACKOFF_SEC) - 1)]
            time.sleep(backoff)

    def _attach(self, offset: int, deadline: float | None) -> Iterator[StreamEvent]:
        """One attach attempt. Ends when the body ends, for any reason.

        The read timeout is a silence bound, not a duration bound: the daemon sends
        an SSE keepalive every 15 seconds by default, so silence for `idle_timeout`
        means the connection is dead even though a live stream may legitimately
        produce no output for hours. Without it a half-open connection — the
        failure a NAT or proxy timeout produces, where no FIN ever arrives — hangs
        this iterator forever and the reconnect logic never gets to run.
        """
        parser = SseParser()
        with self._session.transport.stream(
            "GET",
            f"/v1/exec/{self.exec_id}/stream",
            params={"offset": offset},
            timeout=httpx.Timeout(self._session.timeout, read=self.idle_timeout),
        ) as response:
            for chunk in response.iter_bytes():
                for frame in parser.feed(chunk):
                    event = _decode(frame.event, frame.data)
                    if event is not None:
                        yield event
                if deadline is not None and time.monotonic() >= deadline:
                    return

    def write_stdin(self, data: bytes, *, eof: bool = False) -> StdinAck:
        """Writes to the child's stdin. Requires the exec to have been started with
        `stdin=True`, or the daemon answers 409 (`Conflict`).

        `eof` in the same call is the common case for feeding a prompt: two round
        trips would leave a window where the child has the bytes but not the EOF
        that tells it the input is complete.
        """
        body: dict[str, object] = {}
        if data:
            body["data_b64"] = base64.b64encode(data).decode("ascii")
        if eof:
            body["signal"] = "eof"
        response = self._session.transport.send("POST", f"/v1/exec/{self.exec_id}/stdin", json=body)
        return StdinAck.from_json(response.json())

    def close_stdin(self) -> StdinAck:
        """Sends EOF. Nothing else closes it: the daemon's copy of the pipe outlives
        `Child::wait()`, so a child blocked reading stdin hangs until its timeout
        unless someone calls this."""
        return self.write_stdin(b"", eof=True)

    def ack(self) -> ExecResult:
        """Releases the buffered output and starts the TTL clock.

        409 (`Conflict`) means either the exec has not exited — output is still
        being written — or an earlier ack already took it. Both are real states, not
        the same state, and the detail string distinguishes them.
        """
        response = self._session.transport.send("POST", f"/v1/exec/{self.exec_id}/ack")
        return ExecResult.from_json(response.json())

    def kill(self) -> bool:
        """Signals the whole process group, not just the direct child.

        Returns whether anything was signaled. `False` means no pgid was ever
        captured, i.e. the child had already been reaped.
        """
        response = self._session.transport.send("POST", f"/v1/exec/{self.exec_id}/kill")
        return bool(response.json().get("killed", False))

    def wait_and_ack(self, timeout: float = 300.0) -> ExecResult:
        """Waits, then acks, returning the result that carries the output.

        Which result is returned matters: the ack response carries the released
        output, and a poll issued after the ack reports `phase: acked` with no
        output at all. Returning the wrong one of the two is a silent empty-output
        bug, so the sequencing lives here rather than at every call site.
        """
        done = self.wait(timeout=timeout)
        if done.phase is Phase.ACKED:
            return done
        return self.ack()


def _decode(name: str, data: object) -> StreamEvent | None:
    """Maps one SSE frame onto a typed event, ignoring anything unrecognized."""
    if not isinstance(data, dict):
        return None
    if name == "output":
        raw = data.get("output", "")
        try:
            decoded = base64.b64decode(raw, validate=True)
        except (ValueError, TypeError):
            return None
        return OutputChunk(
            stream=StreamKind(data.get("stream", "stdout")),
            offset=int(data.get("offset", 0)),
            data=decoded,
        )
    if name == "gap":
        return Gap(start=int(data.get("from", 0)), end=int(data.get("to", 0)))
    if name == "exit":
        return Exit.from_json(data)
    return None
