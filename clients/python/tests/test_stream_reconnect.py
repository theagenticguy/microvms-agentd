"""Reconnect-at-offset: the reason the offset exists and the reason this file does.

Without a cursor, a dropped stream leaves two bad options — lose everything after
the drop, or replay from zero. E2B's equivalent is broken precisely because it has
no offset. So the properties asserted here are:

  1. a cut body reconnects, and asks to resume at the last byte actually delivered;
  2. no byte is delivered twice and none is skipped across the seam;
  3. a stream that ended *with* an exit event does not reconnect;
  4. a gap is a visible, typed event rather than a silent skip.
"""

from __future__ import annotations

import pytest

from fake_daemon import Route, chunked_writer, exit_frame, output_frame, sse
from microvms_agentd.errors import AgentdError, NotFound, OutputGap, TransportError
from microvms_agentd.models import Exit, Gap, OutputChunk
from microvms_agentd.session import Session


def test_a_cut_stream_reconnects_at_the_last_delivered_offset(daemon) -> None:
    # First attach delivers "hello" (5 bytes) then the socket dies with no exit
    # event. The reconnect must ask for offset=5: not 0, which would replay, and
    # not 11, which would skip.
    daemon.on(
        "GET",
        "/v1/exec/e1/stream",
        Route(writer=chunked_writer([output_frame(0, "hello")], cut=True)),
        Route(writer=chunked_writer([output_frame(5, " world"), exit_frame(11)])),
    )

    with Session(endpoint=daemon.url, agent_token="t") as session:
        events = list(session.exec("e1").stream())

    text = b"".join(e.data for e in events if isinstance(e, OutputChunk))
    assert text == b"hello world", "no byte lost and none duplicated across the seam"
    assert isinstance(events[-1], Exit)

    attaches = daemon.calls("GET", "/v1/exec/e1/stream")
    assert len(attaches) == 2
    assert attaches[0].query["offset"] == ["0"]
    assert attaches[1].query["offset"] == ["5"], "resumed at the byte after the last delivered"


def test_several_drops_in_a_row_keep_advancing_the_cursor(daemon) -> None:
    daemon.on(
        "GET",
        "/v1/exec/e2/stream",
        Route(writer=chunked_writer([output_frame(0, "aa")], cut=True)),
        Route(writer=chunked_writer([output_frame(2, "bb")], cut=True)),
        Route(writer=chunked_writer([output_frame(4, "cc"), exit_frame(6)])),
    )
    with Session(endpoint=daemon.url, agent_token="t") as session:
        events = list(session.exec("e2").stream())

    assert b"".join(e.data for e in events if isinstance(e, OutputChunk)) == b"aabbcc"
    offsets = [c.query["offset"][0] for c in daemon.calls("GET", "/v1/exec/e2/stream")]
    assert offsets == ["0", "2", "4"]


def test_a_drop_with_nothing_delivered_resumes_at_zero(daemon) -> None:
    # A connection that dies before any output must not advance the cursor, or the
    # first bytes of the command are lost forever.
    daemon.on(
        "GET",
        "/v1/exec/e3/stream",
        Route(writer=chunked_writer([], cut=True)),
        Route(writer=chunked_writer([output_frame(0, "first"), exit_frame(5)])),
    )
    with Session(endpoint=daemon.url, agent_token="t") as session:
        events = list(session.exec("e3").stream())
    assert b"".join(e.data for e in events if isinstance(e, OutputChunk)) == b"first"
    offsets = [c.query["offset"][0] for c in daemon.calls("GET", "/v1/exec/e3/stream")]
    assert offsets == ["0", "0"]


def test_a_clean_exit_does_not_reconnect(daemon) -> None:
    # The distinction SSE framing buys: a body that closes *with* an exit event is
    # a finished command, and reattaching would hang on an exec that is over.
    daemon.on(
        "GET",
        "/v1/exec/e4/stream",
        Route(writer=chunked_writer([output_frame(0, "done"), exit_frame(4)])),
    )
    with Session(endpoint=daemon.url, agent_token="t") as session:
        events = list(session.exec("e4").stream())
    assert len(daemon.calls("GET", "/v1/exec/e4/stream")) == 1
    assert isinstance(events[-1], Exit)
    assert events[-1].offset == 4


def test_reconnect_off_raises_on_a_cut_rather_than_ending_quietly(daemon) -> None:
    # Opting out of recovery must not also opt out of *knowing*. Ending the
    # iterator silently would make a truncated stream indistinguishable from a
    # complete one, which is the defect the exit event exists to prevent — so the
    # chunks that did arrive are yielded and then the drop surfaces.
    daemon.on(
        "GET",
        "/v1/exec/e5/stream",
        Route(writer=chunked_writer([output_frame(0, "partial")], cut=True)),
    )
    seen: list[OutputChunk] = []
    with Session(endpoint=daemon.url, agent_token="t") as session, pytest.raises(TransportError):
        for event in session.exec("e5").stream(reconnect=False):
            if isinstance(event, OutputChunk):
                seen.append(event)
    assert len(daemon.calls("GET", "/v1/exec/e5/stream")) == 1
    assert b"".join(e.data for e in seen) == b"partial", "delivered before the drop was raised"


def test_endless_drops_give_up_with_the_last_good_offset_named(daemon) -> None:
    daemon.on(
        "GET",
        "/v1/exec/e6/stream",
        Route(writer=chunked_writer([output_frame(0, "xy")], cut=True)),
    )
    with (
        Session(endpoint=daemon.url, agent_token="t") as session,
        pytest.raises(AgentdError) as caught,
    ):
        list(session.exec("e6").stream(max_reconnects=2))
    assert "offset 2" in str(caught.value)


def test_a_gap_is_yielded_as_a_typed_event(daemon) -> None:
    # The daemon reports a gap instead of handing back a window that quietly starts
    # later than asked. A client that swallowed it would read a truncated log as a
    # complete one.
    daemon.on(
        "GET",
        "/v1/exec/e7/stream",
        Route(
            writer=chunked_writer(
                [sse("gap", {"from": 0, "to": 100}), output_frame(100, "tail"), exit_frame(104)]
            )
        ),
    )
    with Session(endpoint=daemon.url, agent_token="t") as session:
        events = list(session.exec("e7").stream())

    gaps = [e for e in events if isinstance(e, Gap)]
    assert len(gaps) == 1
    assert (gaps[0].start, gaps[0].end, gaps[0].size) == (0, 100, 100)


def test_raise_on_gap_turns_it_into_an_exception(daemon) -> None:
    daemon.on(
        "GET",
        "/v1/exec/e8/stream",
        Route(writer=chunked_writer([sse("gap", {"from": 5, "to": 9}), exit_frame(9)])),
    )
    with (
        Session(endpoint=daemon.url, agent_token="t") as session,
        pytest.raises(OutputGap) as caught,
    ):
        list(session.exec("e8").stream(raise_on_gap=True))
    assert (caught.value.start, caught.value.end) == (5, 9)


def test_a_gap_advances_the_cursor_so_a_reconnect_does_not_re_request_it(daemon) -> None:
    # Those bytes are evicted. Resuming before them would be told about the same gap
    # again, forever.
    daemon.on(
        "GET",
        "/v1/exec/e9/stream",
        Route(writer=chunked_writer([sse("gap", {"from": 0, "to": 4096})], cut=True)),
        Route(writer=chunked_writer([output_frame(4096, "after"), exit_frame(4101)])),
    )
    with Session(endpoint=daemon.url, agent_token="t") as session:
        list(session.exec("e9").stream())
    offsets = [c.query["offset"][0] for c in daemon.calls("GET", "/v1/exec/e9/stream")]
    assert offsets == ["0", "4096"]


def test_a_frame_split_across_two_socket_writes_is_reassembled(daemon) -> None:
    # The same property as the parser unit test, but over a real socket: the two
    # halves land as separate reads, and the frame must still arrive whole.
    whole = output_frame(0, "split-across-reads")
    daemon.on(
        "GET",
        "/v1/exec/e10/stream",
        Route(writer=chunked_writer([whole[:20], whole[20:], exit_frame(18)])),
    )
    with Session(endpoint=daemon.url, agent_token="t") as session:
        events = list(session.exec("e10").stream())
    assert b"".join(e.data for e in events if isinstance(e, OutputChunk)) == b"split-across-reads"


def test_a_collected_exec_is_fatal_rather_than_reconnected_forever(daemon) -> None:
    # An acked entry past its TTL is gone. Reconnecting can never succeed, so 404 is
    # fatal even with reconnect on — the alternative is a loop that never ends.
    daemon.on("GET", "/v1/exec/e11/stream", Route(status=404, body=b"unknown_exec"))
    with Session(endpoint=daemon.url, agent_token="t") as session, pytest.raises(NotFound):
        list(session.exec("e11").stream())
    assert len(daemon.calls("GET", "/v1/exec/e11/stream")) == 1


def test_stderr_and_stdout_share_one_offset_space(daemon) -> None:
    # One cursor, not two that can disagree about ordering. A client holding two
    # would have to invent an interleaving the daemon already decided.
    daemon.on(
        "GET",
        "/v1/exec/e12/stream",
        Route(
            writer=chunked_writer(
                [
                    output_frame(0, "out", "stdout"),
                    output_frame(3, "err", "stderr"),
                    exit_frame(6),
                ]
            )
        ),
    )
    with Session(endpoint=daemon.url, agent_token="t") as session:
        events = [e for e in session.exec("e12").stream() if isinstance(e, OutputChunk)]
    assert [(e.stream.value, e.offset, e.end) for e in events] == [
        ("stdout", 0, 3),
        ("stderr", 3, 6),
    ]
