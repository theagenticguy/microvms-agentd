# SPDX-License-Identifier: Apache-2.0
"""The exec and stream surface: `src/exec.rs`.

# What is covered offline, and what is not

An `ExecHandle` is an id plus a transport, so *constructing* one needs no daemon — and the
`stream()` path can be driven against the loopback SSE server in `conftest.py`, which is what
makes this the one binding module with real behavioural coverage rather than argument checks
alone. The tests below fall into three groups:

* **Construction and argument validation** — no server at all. Every numeric parameter goes
  through the core's `duration_of_secs_f64`, and `NaN`/`inf`/negative all have to be refused
  before a request is built.
* **The event objects** — the three classes, their `kind` tags, and the byte/offset arithmetic
  a caller resumes from. Driven through the SSE server, because that is the only way to obtain
  one: none of the three has a constructor.
* **The stream contract as the binding reaches it** — order, the terminal event, a `Gap` as an
  event versus as an exception, `break` stopping the drive, and a cut reconnecting at the
  cursor. These are `microvms-core` properties, and asserting them *here* is the point: the
  binding drives them through its own spawned task and capacity-1 channel, so "core's tests
  pass" is not a statement about this path.

**The boundary, stated honestly.** The SSE server is this suite's own transcription of the frame
shapes `microvms-core/src/session/sse.rs` parses. It is not `agentd`. So nothing here proves the
daemon emits those frames — if the daemon's framing changed, these tests would stay green while
the conformance suite went red. What they do prove is that this client parses, orders, and
resumes correctly given the framing, which is the half that lives in this repository's bindings.

The methods that genuinely need a live daemon (`poll`, `wait`, `ack`, `kill`, `write_stdin`) are
covered only for their argument validation, and the reason is that a fake for them would be a
fake of the daemon's *state machine* — a second implementation whose agreement with the real one
is exactly what nobody would be testing.
"""

from __future__ import annotations

import pytest

import microvms
from conftest import exit_frame, gap_frame, output_frame

# A syntactically valid exec id, in the `x-<16 hex>` shape the client mints.
EXEC_ID = "x-0000000000000001"


def handle_against(server: object) -> microvms.ExecHandle:
    """An exec handle addressing the scripted server."""
    session = microvms.Session.direct(server.endpoint, "agent-token")  # type: ignore[attr-defined]
    return session.exec(EXEC_ID)


def offline_handle() -> microvms.ExecHandle:
    """A handle whose endpoint nothing is listening on.

    Fine for every argument-validation test: the refusals below all happen *before* a request is
    built, which is the property being asserted.
    """
    return microvms.Session.direct("http://127.0.0.1:9", "agent-token").exec(EXEC_ID)


# -- construction, with no daemon anywhere ------------------------------------


def test_a_handle_is_an_id_plus_a_transport_and_reaches_nothing_to_exist() -> None:
    """The reattach path, which is why the exec id is caller-minted.

    A constructor that probed would make "do I have a handle" mean "does that exec exist",
    which are different questions — and the second one has no answer during the window between
    a `run` and the daemon recording it.
    """
    handle = offline_handle()
    assert handle.exec_id == EXEC_ID
    assert EXEC_ID in repr(handle)


def test_two_handles_for_one_id_address_the_same_exec() -> None:
    """The idempotency key survives a rebuild, including across a process restart.

    Asserted as far as it can be offline: the id is what addresses the exec, so two handles
    carrying it are equivalent by construction. A binding that minted its own id per handle
    would break the reattach and nothing local would object.
    """
    session = microvms.Session.direct("http://127.0.0.1:9", "agent-token")
    assert session.exec(EXEC_ID).exec_id == session.exec(EXEC_ID).exec_id


@pytest.mark.parametrize(
    "bad", [-1.0, -0.001, float("nan"), float("inf"), float("-inf")]
)
def test_the_stream_idle_timeout_is_refused_by_the_core_before_any_request(
    bad: float,
) -> None:
    """`NaN` and `inf` matter as much as a negative, and Python reaches them by accident.

    `float("inf")` arrives from a division; `NaN` from `0/0` or a parsed empty field. An idle
    timeout of `NaN` compares false against every deadline, so a stream would never time out —
    a hang rather than an error, which is the worst way for a bad argument to fail.

    Refused by the *core*'s `duration_of_secs_f64` rather than by a check in the binding, which
    is the BIND-2 rule: the refusal and its message stay in one place.
    """
    with pytest.raises(microvms.InvalidArgError) as raised:
        offline_handle().stream(idle_timeout=bad)
    assert raised.value.code == "ERR_INVALID_ARG"
    assert raised.value.wire_kind is None, "nothing reached the daemon"


@pytest.mark.parametrize("bad", [-1.0, float("nan"), float("inf")])
def test_every_timeout_parameter_on_the_surface_shares_the_one_refusal(
    bad: float,
) -> None:
    """Four entry points, one check, so none of them is the loose one.

    A binding that validated `stream(idle_timeout=)` and not `wait(timeout=)` would leave the
    laxer door as the one most callers use — and `wait` is the more commonly passed of the two.
    """
    handle = offline_handle()
    for call in (
        lambda: handle.stream(idle_timeout=bad),
        lambda: handle.wait(timeout=bad),
        lambda: handle.wait_and_ack(timeout=bad),
    ):
        with pytest.raises(microvms.InvalidArgError) as raised:
            call()
        assert raised.value.code == "ERR_INVALID_ARG"


def test_a_zero_timeout_is_accepted_because_it_means_do_not_wait() -> None:
    """The boundary the refusals must not swallow.

    Zero is a real request — poll once and give up — and refusing it would force a caller to
    special-case "I do not want to block".
    """
    stream = offline_handle().stream(idle_timeout=0.0)
    assert stream is not None


def test_the_stream_options_are_all_keyword_only_so_none_can_be_transposed() -> None:
    """Five parameters, four of them numbers or flags.

    Positionally, `stream(0, False, 20, False, 60.0)` is unreadable and one transposition away
    from `max_reconnects=60` with a 20-second idle timeout — both plausible, neither what was
    meant. Keyword-only makes the transposition unwriteable.
    """
    handle = offline_handle()
    with pytest.raises(TypeError):
        handle.stream(0)  # type: ignore[misc]
    # And every name really is accepted as a keyword.
    assert handle.stream(
        offset=5,
        reconnect=False,
        max_reconnects=3,
        error_on_gap=True,
        idle_timeout=1.0,
    )


# -- the event objects ---------------------------------------------------------


def test_an_output_chunk_reports_its_bytes_its_offset_and_where_a_cursor_resumes(
    sse_server: object,
) -> None:
    """`end` is `offset + len(data)`, which is the number a resume passes back.

    Derived rather than stored, so the arithmetic is what is under test. A caller that computed
    it themselves would be maintaining a second cursor — the thing the core's docs single out as
    the way the two come to disagree exactly when a reconnect happens.
    """
    server = sse_server([[output_frame(64, b"hello\n"), exit_frame(70)]])  # type: ignore[operator]
    events = list(handle_against(server).stream(offset=64, idle_timeout=5.0))

    chunk = events[0]
    assert chunk.kind == "output"
    assert chunk.stream == "stdout"
    assert chunk.offset == 64
    assert chunk.data == b"hello\n"
    assert chunk.end == 64 + len(b"hello\n") == 70
    assert isinstance(chunk.data, bytes), "output is arbitrary bytes, never str"


def test_output_data_is_bytes_and_text_is_the_lossy_step_a_reader_sees(
    sse_server: object,
) -> None:
    """A method, not a getter, because the decode is lossy and the caller should see it.

    Exec output is arbitrary bytes — a compiler writing a latin-1 filename, a program emitting a
    partial UTF-8 sequence at a chunk boundary. Decoding in `data` would make the loss invisible;
    `text()` is one visible call.
    """
    invalid = b"caf\xe9\n"
    server = sse_server([[output_frame(0, invalid), exit_frame(len(invalid))]])  # type: ignore[operator]
    chunk = list(handle_against(server).stream(idle_timeout=5.0))[0]

    assert chunk.data == invalid
    # Lossy, and *replacing* rather than raising: a stream must not die on one bad byte.
    assert chunk.text() == "caf�\n"
    assert chunk.end == len(invalid)


def test_a_gap_is_a_typed_event_carrying_the_range_that_is_gone(
    sse_server: object,
) -> None:
    """A truncated log has to be distinguishable from a complete one.

    That is the whole argument for a typed event over a log line: `start` inclusive, `end`
    exclusive, so `end` is where a cursor resumes and `size` is how much was lost.
    """
    server = sse_server(  # type: ignore[operator]
        [
            [
                output_frame(0, b"AA"),
                gap_frame(2, 900),
                output_frame(900, b"ZZ"),
                exit_frame(902),
            ]
        ]
    )
    events = list(handle_against(server).stream(idle_timeout=5.0))

    gaps = [event for event in events if event.kind == "gap"]
    assert len(gaps) == 1
    assert gaps[0].start == 2
    assert gaps[0].end == 900
    assert gaps[0].size == 898
    assert isinstance(gaps[0], microvms.Gap)
    # The stream continues past the gap rather than ending on it.
    assert [event.kind for event in events] == ["output", "gap", "output", "exit"]


def test_the_exit_event_carries_a_total_rather_than_a_resume_position(
    sse_server: object,
) -> None:
    """`offset` on an exit is a **total**, and treating it as a cursor would replay from the end.

    A cursor never moves past an exit for exactly that reason, and this is the
    binding-side statement of it: `Exit` has `offset` and no `end`.
    """
    server = sse_server([[output_frame(0, b"done\n"), exit_frame(5)]])  # type: ignore[operator]
    exit_event = list(handle_against(server).stream(idle_timeout=5.0))[-1]

    assert isinstance(exit_event, microvms.Exit)
    assert exit_event.kind == "exit"
    assert exit_event.offset == 5
    assert exit_event.exit_code == 0
    assert exit_event.signal is None
    assert not hasattr(exit_event, "end"), "an exit offset is not a resume position"


def test_a_signal_death_has_no_exit_code_and_zero_is_not_no_signal(
    sse_server: object,
) -> None:
    """Two `None`s that are not interchangeable with zero.

    A child killed by SIGKILL has no exit code at all, and reporting `0` there would say it
    succeeded. Symmetrically, `signal=0` is not "no signal" — which is why both are `None`
    rather than sentinel integers.
    """
    server = sse_server([[exit_frame(0, exit_code=None, signal=9)]])  # type: ignore[operator]
    exit_event = list(handle_against(server).stream(idle_timeout=5.0))[0]

    assert exit_event.exit_code is None
    assert exit_event.signal == 9


def test_the_three_event_classes_are_distinct_so_isinstance_and_kind_both_work(
    sse_server: object,
) -> None:
    """Both branching styles, because Python callers reach for either.

    `kind` is the tag for a `match`; `isinstance` is the tag for someone who prefers classes.
    The pairing has to be exact — a `Gap` reporting `kind == "output"` would send a truncation
    down the happy path.
    """
    server = sse_server(  # type: ignore[operator]
        [
            [
                output_frame(0, b"AA"),
                gap_frame(2, 4),
                output_frame(4, b"BB"),
                exit_frame(6),
            ]
        ]
    )
    events = list(handle_against(server).stream(idle_timeout=5.0))

    expected = {
        "output": microvms.OutputChunk,
        "gap": microvms.Gap,
        "exit": microvms.Exit,
    }
    for event in events:
        assert isinstance(event, expected[event.kind]), event
        # And not an instance of either of the others, so the three are really separate types.
        for kind, cls in expected.items():
            if kind != event.kind:
                assert not isinstance(event, cls)


def test_no_event_class_can_be_constructed_by_a_caller(sse_server: object) -> None:
    """Events come off a stream or not at all.

    A constructible `Exit` is a caller able to synthesise the one event that means "the command
    finished" — the distinction the whole SSE framing exists to carry — and a test double built
    that way would assert against a value nothing produced.
    """
    for cls in (microvms.OutputChunk, microvms.Gap, microvms.Exit):
        with pytest.raises(TypeError):
            cls()  # type: ignore[call-arg]


# -- the stream contract, as the binding drives it ----------------------------


def test_events_reach_the_iterator_in_wire_order(sse_server: object) -> None:
    """Order, asserted on the offsets rather than only on the reassembled bytes.

    Two chunks concatenated the wrong way round still total the right length, so a byte-count
    check would miss a reordering. This is the property the binding's capacity-1 channel could
    plausibly break — a buffered hand-off, or a second consumer task — and reordering a child's
    stdout is not a subtle failure for whoever reads it.
    """
    server = sse_server(  # type: ignore[operator]
        [
            [
                output_frame(0, b"AAAAA"),
                output_frame(5, b"BBBBB"),
                output_frame(10, b"CCCCC"),
                exit_frame(15),
            ]
        ]
    )
    events = list(handle_against(server).stream(idle_timeout=5.0))

    assert [event.offset for event in events if event.kind == "output"] == [0, 5, 10]
    assert b"".join(e.data for e in events if e.kind == "output") == b"AAAAABBBBBCCCCC"
    assert events[-1].kind == "exit", (
        "the terminal event has to be delivered, not swallowed"
    )


def test_the_iterator_ends_after_the_terminal_event_rather_than_hanging(
    sse_server: object,
) -> None:
    """`StopIteration` follows the exit event, which is what makes a `for` loop terminate.

    Worth its own test because the failure mode is a hang: a channel whose sender was never
    dropped would leave `__next__` blocked on `recv` forever, and a hang in a `for` loop reads
    as a slow daemon rather than as a client bug.
    """
    server = sse_server([[output_frame(0, b"hi\n"), exit_frame(3)]])  # type: ignore[operator]
    stream = handle_against(server).stream(idle_timeout=5.0)

    kinds = [event.kind for event in stream]
    assert kinds == ["output", "exit"]
    # Drained, and it stays drained.
    with pytest.raises(StopIteration):
        next(stream)


def test_the_stream_is_its_own_iterator_so_a_for_loop_takes_it_directly(
    sse_server: object,
) -> None:
    """`__iter__` answers `self`, which is the iterator protocol a `for` loop needs."""
    server = sse_server([[exit_frame(0)]])  # type: ignore[operator]
    stream = handle_against(server).stream(idle_timeout=5.0)
    assert iter(stream) is stream


def test_breaking_out_of_the_loop_stops_the_stream_rather_than_leaving_it_reading(
    sse_server: object,
) -> None:
    """The `Break` path, which is why the driver's callback returns a `ControlFlow`.

    A `break` drops the iterator, the next channel send fails, and the drive ends — so nothing is
    left reading a body nobody reads. Asserted through the observable that shows it: only one
    attach was made, and the loop stopped after one event even though three were scripted.
    """
    server = sse_server(  # type: ignore[operator]
        [[output_frame(0, b"first\n"), output_frame(6, b"second\n"), exit_frame(13)]]
    )
    seen = 0
    for _ in handle_against(server).stream(idle_timeout=5.0):
        seen += 1
        break

    assert seen == 1
    assert len(server.requested_paths) == 1, "a stopped stream reattached"  # type: ignore[attr-defined]


def test_a_cut_stream_reconnects_at_the_cursor_losing_and_duplicating_nothing(
    sse_server: object,
) -> None:
    """The reconnect property, through the binding's own task and channel.

    The verdict is the reassembled bytes **and** the offset the second attach asked for, and the
    second half is what makes it a real test: a client that reconnected at zero would deliver
    every byte too, and only the seam shows the difference.

    This is also the regression for the driver migration. The binding used to consume a `Stream`;
    it now drives `for_each_event_async`, and the cursor is read off core's state machine rather
    than tallied here — so a migration that had dropped the cursor would show up exactly as a
    second `offset=0`.
    """
    server = sse_server(  # type: ignore[operator]
        [
            # First attach: two frames, then the body ends with no exit event — a cut.
            [output_frame(0, b"AAAA\n"), output_frame(5, b"BBBB\n")],
            # Second attach: the daemon replays from the offset it was asked for.
            [output_frame(10, b"CCCC\n"), exit_frame(15)],
        ]
    )
    events = list(handle_against(server).stream(idle_timeout=5.0))

    assert (
        b"".join(e.data for e in events if e.kind == "output") == b"AAAA\nBBBB\nCCCC\n"
    )
    assert events[-1].kind == "exit"
    assert server.offsets_requested() == [0, 10], (
        "the reconnect asked for the wrong byte"
    )  # type: ignore[attr-defined]


def test_a_gap_advances_the_cursor_so_a_reconnect_does_not_ask_for_evicted_bytes(
    sse_server: object,
) -> None:
    """The second cursor rule, and the one a locally-tallied cursor gets wrong.

    The daemon has already moved past the evicted range; if this client's cursor did not follow,
    a reconnect would ask for those bytes again and be told about the same gap forever — a
    livelock that looks like a slow stream rather than an error.
    """
    server = sse_server(  # type: ignore[operator]
        [
            [output_frame(0, b"AA"), gap_frame(2, 900)],
            [output_frame(900, b"ZZ"), exit_frame(902)],
        ]
    )
    events = list(handle_against(server).stream(idle_timeout=5.0))

    assert [event.kind for event in events] == ["output", "gap", "output", "exit"]
    assert server.offsets_requested() == [0, 900]  # type: ignore[attr-defined]


def test_reconnect_off_ends_the_stream_at_the_cut_without_an_exit_event(
    sse_server: object,
) -> None:
    """For a caller doing its own reconnection, and the ending is *silent* rather than an error.

    The load-bearing part is what a caller can tell afterwards: the iterator ended and no `Exit`
    event arrived, which is the signature of a cut. A caller that treated "the loop finished" as
    "the command finished" would pass a CI step on output it never received — so the absence of
    the terminal event is the assertion.
    """
    server = sse_server([[output_frame(0, b"partial")]])  # type: ignore[operator]
    events = list(handle_against(server).stream(reconnect=False, idle_timeout=5.0))

    assert [event.kind for event in events] == ["output"]
    assert not any(isinstance(event, microvms.Exit) for event in events)
    assert len(server.requested_paths) == 1, "reconnect=False reattached anyway"  # type: ignore[attr-defined]


def test_a_starting_offset_is_passed_through_to_the_daemon(sse_server: object) -> None:
    """What a second process resuming another's stream passes.

    The offset a caller supplies has to reach the query string unaltered: a client that started
    at zero regardless would replay output the first process already showed someone.
    """
    server = sse_server([[output_frame(64, b"tail"), exit_frame(68)]])  # type: ignore[operator]
    events = list(handle_against(server).stream(offset=64, idle_timeout=5.0))

    assert [event.data for event in events if event.kind == "output"] == [b"tail"]
    assert server.offsets_requested() == [64]  # type: ignore[attr-defined]


def test_error_on_gap_raises_the_typed_error_instead_of_yielding_a_gap_event(
    sse_server: object,
) -> None:
    """What a caller that must have complete output asks for.

    Two things are asserted and the second is the interesting one: the events *before* the gap
    stay delivered. That asymmetry is deliberate — the bytes a caller already received are real
    output, and there is nothing to unwind them with — so the exception arrives after them
    rather than instead of them.
    """
    server = sse_server([[output_frame(0, b"AA"), gap_frame(2, 900)]])  # type: ignore[operator]
    stream = handle_against(server).stream(error_on_gap=True, idle_timeout=5.0)

    before = next(stream)
    assert before.kind == "output"
    assert before.data == b"AA"

    with pytest.raises(microvms.MicrovmError) as raised:
        next(stream)
    # The daemon-status class, which is how a caller tells this from any other failure.
    assert raised.value.wire_kind == "OutputGap"
    assert "[2, 900)" in str(raised.value), str(raised.value)


def test_a_stream_error_is_raised_out_of_the_iterator_rather_than_ending_it_silently() -> (
    None
):
    """A silent end would read as complete output, which is the failure to avoid.

    No server here at all: a connection refused is the simplest real transport failure, and it
    reaches the caller as an exception carrying the retryable flag — because the exec is still
    alive server-side and the request can be repeated.
    """
    with pytest.raises(microvms.MicrovmError) as raised:
        list(offline_handle().stream(reconnect=False, idle_timeout=1.0))
    assert raised.value.retryable is True, (
        "a refused connection says nothing about the exec"
    )
    assert raised.value.code == "ERR_RETRYABLE"


def test_two_streams_over_one_handle_each_get_their_own_events(
    sse_server: object,
) -> None:
    """Each `stream()` call is a fresh drive with its own task and channel.

    A shared channel would mean two iterators splitting one event sequence between them — each
    seeing roughly half the output, with nothing raised anywhere. The reattach case makes this
    concrete: the same handle is what a caller uses to resume.
    """
    server = sse_server(  # type: ignore[operator]
        [
            [output_frame(0, b"first\n"), exit_frame(6)],
            [output_frame(0, b"second\n"), exit_frame(7)],
        ]
    )
    handle = handle_against(server)

    first = [
        event.data
        for event in handle.stream(idle_timeout=5.0)
        if event.kind == "output"
    ]
    second = [
        event.data
        for event in handle.stream(idle_timeout=5.0)
        if event.kind == "output"
    ]

    assert first == [b"first\n"]
    assert second == [b"second\n"]
    assert len(server.requested_paths) == 2  # type: ignore[attr-defined]


def test_a_stream_survives_more_events_than_the_channel_can_hold(
    sse_server: object,
) -> None:
    """The backpressure path, which is the reason core grew an async callback driver.

    The binding's channel holds **one** event, so a stream of many frames means the driver waits
    on `send` for all but the first — which is the case the old synchronous callback could not
    serve, because its only available send would have parked the runtime worker the driver runs
    on. Sixty-four frames is comfortably more than the bound; every one of them arrives, in
    order, with the terminal event last.

    **Falsification** — this is the test that goes red if the drive is ever changed to drop
    events under a full channel (a `try_send` in place of the awaited `send`): the count and the
    offsets both break.
    """
    frames = [output_frame(index * 2, b"xy") for index in range(64)]
    frames.append(exit_frame(128))
    server = sse_server([frames])  # type: ignore[operator]

    events = list(handle_against(server).stream(idle_timeout=5.0))
    outputs = [event for event in events if event.kind == "output"]

    assert len(outputs) == 64, "events were dropped under backpressure"
    assert [event.offset for event in outputs] == [index * 2 for index in range(64)]
    assert b"".join(event.data for event in outputs) == b"xy" * 64
    assert events[-1].kind == "exit"


def test_an_unknown_event_name_is_skipped_rather_than_ending_the_stream(
    sse_server: object,
) -> None:
    """Forward compatibility, and the absence of a spurious end-of-stream.

    A daemon that grows a fourth event type must not truncate this client's output. A frame that
    decodes to nothing is skipped, so the events either side of it still arrive — and critically,
    the unknown frame does not read as the body ending.
    """
    server = sse_server(  # type: ignore[operator]
        [
            [
                output_frame(0, b"AA"),
                b'event: something-new\ndata: {"whatever":1}\n\n',
                output_frame(2, b"BB"),
                exit_frame(4),
            ]
        ]
    )
    events = list(handle_against(server).stream(idle_timeout=5.0))

    assert [event.kind for event in events] == ["output", "output", "exit"]
    assert b"".join(e.data for e in events if e.kind == "output") == b"AABB"
    assert len(server.requested_paths) == 1, "an unknown frame was read as a cut"  # type: ignore[attr-defined]


# -- the result object ---------------------------------------------------------


def test_an_exec_result_has_no_constructor_so_it_only_comes_from_the_daemon() -> None:
    """A result is an observation, not a value a caller builds.

    A constructible `ExecResult` is a caller able to assert success about a command nobody ran.
    """
    with pytest.raises(TypeError):
        microvms.ExecResult()  # type: ignore[call-arg]
    with pytest.raises(TypeError):
        microvms.StdinAck()  # type: ignore[call-arg]


def test_the_phase_and_stream_spellings_are_the_ones_the_constants_publish() -> None:
    """The two closed sets a caller branches on, published so nothing hardcodes a spelling.

    Asserted through `session_constants()` because that is what a consumer validates against,
    and the event objects above are what they receive — the two have to use one vocabulary.
    """
    constants = microvms.session_constants()
    assert constants["phases"] == ["running", "exited", "acked"]
    assert constants["streamKinds"] == ["stdout", "stderr"]


def test_a_stderr_chunk_reports_its_own_stream_in_the_shared_offset_space(
    sse_server: object,
) -> None:
    """One offset space for both streams, which is why a caller holds one cursor.

    Two cursors could disagree about ordering — and the interleaving of stdout and stderr *is*
    the information someone reading a build log needs. So `stream` is a label on the chunk, not
    a separate sequence.
    """
    server = sse_server(  # type: ignore[operator]
        [
            [
                output_frame(0, b"out\n", stream="stdout"),
                output_frame(4, b"err\n", stream="stderr"),
                exit_frame(8),
            ]
        ]
    )
    outputs = [
        event
        for event in handle_against(server).stream(idle_timeout=5.0)
        if event.kind == "output"
    ]

    assert [event.stream for event in outputs] == ["stdout", "stderr"]
    # Contiguous across the two streams, which is the shared-space claim.
    assert [(event.offset, event.end) for event in outputs] == [(0, 4), (4, 8)]
