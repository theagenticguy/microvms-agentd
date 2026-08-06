"""Wire shapes, mirroring `agentd/src/exec.rs` field for field.

Every field here exists in the daemon. Nothing is invented, and nothing is
renamed: a client that pretties up `writers_may_be_alive` into something friendlier
makes the daemon's own logs and this library's objects impossible to correlate.

Unknown keys are kept in `extra` rather than dropped, so a daemon newer than this
library does not silently lose information.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import StrEnum
from typing import Any


class Phase(StrEnum):
    """`Phase` in exec.rs. `acked` means the output has already been released."""

    RUNNING = "running"
    EXITED = "exited"
    ACKED = "acked"


class StreamKind(StrEnum):
    """Which pipe a chunk came from. Both share one offset space."""

    STDOUT = "stdout"
    STDERR = "stderr"


@dataclass(frozen=True)
class Health:
    version: str
    bootstrapped: bool

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> Health:
        return cls(
            version=str(data.get("version", "")),
            bootstrapped=bool(data.get("bootstrapped")),
        )


@dataclass(frozen=True)
class ExecResult:
    """A polled `GET /v1/exec/{id}` response.

    `stdout`/`stderr` are absent while running and absent again after an ack — the
    daemon flattens `Outcome` into the response and omits it in both cases. They
    are `None` rather than `""` so a caller can tell "no output" from "output was
    already released", which an empty string cannot express.
    """

    exec_id: str
    phase: Phase
    exit_code: int | None = None
    signal: int | None = None
    stdout: str | None = None
    stderr: str | None = None
    truncated: bool = False
    #: The post-exit linger deadline expired with the pipes still open, so a
    #: grandchild is alive and may write output nobody will ever see. Surfaced
    #: because a harness seeing empty output from a noisy command needs the reason.
    writers_may_be_alive: bool = False
    extra: dict[str, Any] = field(default_factory=dict)

    @property
    def done(self) -> bool:
        return self.phase in (Phase.EXITED, Phase.ACKED)

    @property
    def ok(self) -> bool:
        return self.exit_code == 0

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> ExecResult:
        known = {
            "exec_id",
            "phase",
            "exit_code",
            "signal",
            "stdout",
            "stderr",
            "truncated",
            "writers_may_be_alive",
        }
        return cls(
            exec_id=str(data.get("exec_id", "")),
            phase=Phase(data.get("phase", "running")),
            exit_code=data.get("exit_code"),
            signal=data.get("signal"),
            stdout=data.get("stdout"),
            stderr=data.get("stderr"),
            truncated=bool(data.get("truncated", False)),
            writers_may_be_alive=bool(data.get("writers_may_be_alive", False)),
            extra={k: v for k, v in data.items() if k not in known},
        )


@dataclass(frozen=True)
class OutputChunk:
    """An `output` SSE event, already base64-decoded.

    `offset` is the position of the first byte, so the next resume value is
    `offset + len(data)` — which is what `end` is. That arithmetic lives here
    rather than at every call site, because getting it wrong by one byte is a
    silent duplicate or a silent hole across a reconnect.
    """

    stream: StreamKind
    offset: int
    data: bytes

    @property
    def end(self) -> int:
        return self.offset + len(self.data)

    def text(self, errors: str = "replace") -> str:
        """Decodes as UTF-8. Lossy by default: a chunk boundary can split a
        multi-byte character, and raising there would make streaming unusable for
        any non-ASCII output."""
        return self.data.decode("utf-8", errors)


@dataclass(frozen=True)
class Gap:
    """A `gap` SSE event: bytes in `[start, end)` are gone for good.

    A typed event rather than a log line. The daemon goes out of its way to report
    this instead of handing back a window that quietly starts later than asked, so
    a client that swallowed it would be throwing away the point.
    """

    start: int
    end: int

    @property
    def size(self) -> int:
        return self.end - self.start


@dataclass(frozen=True)
class Exit:
    """The terminal `exit` SSE event.

    A body that closes without one means the *connection* failed, not the command.
    That distinction is why the stream is SSE and not a chunked byte pipe, and it
    is what `stream()` uses to decide whether to reconnect.
    """

    exit_code: int | None
    signal: int | None
    truncated: bool
    writers_may_be_alive: bool
    #: Total bytes published, so a caller can assert it saw all of them.
    offset: int

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> Exit:
        return cls(
            exit_code=data.get("exit_code"),
            signal=data.get("signal"),
            truncated=bool(data.get("truncated", False)),
            writers_may_be_alive=bool(data.get("writers_may_be_alive", False)),
            offset=int(data.get("offset", 0)),
        )


#: What `ExecHandle.stream()` yields.
StreamEvent = OutputChunk | Gap | Exit


@dataclass(frozen=True)
class StdinAck:
    exec_id: str
    written: int
    eof: bool

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> StdinAck:
        return cls(
            exec_id=str(data.get("exec_id", "")),
            written=int(data.get("written", 0)),
            eof=bool(data.get("eof", False)),
        )
