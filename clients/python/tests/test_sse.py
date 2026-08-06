"""SSE frame parsing, including the partial-frame-across-two-reads case.

That case is the reason the parser is incremental. A read boundary is not a frame
boundary — the transport decides where the reads land — so a parser that assumed
otherwise would drop output exactly when the stream got busy.
"""

from __future__ import annotations

from microvms_agentd._sse import SseParser


def test_a_whole_frame_parses_in_one_feed() -> None:
    parser = SseParser()
    frames = list(parser.feed(b'event: output\ndata: {"offset": 0}\n\n'))
    assert len(frames) == 1
    assert frames[0].event == "output"
    assert frames[0].data == {"offset": 0}
    assert parser.pending == 0


def test_a_frame_split_across_two_reads_survives() -> None:
    parser = SseParser()
    first = b'event: output\ndata: {"offset": 0, "str'
    second = b'eam": "stdout", "output": "aGk="}\n\n'

    assert list(parser.feed(first)) == [], "an incomplete frame yields nothing"
    assert parser.pending == len(first), "and is held, not discarded"

    frames = list(parser.feed(second))
    assert len(frames) == 1
    assert frames[0].data["output"] == "aGk="
    assert parser.pending == 0


def test_a_frame_split_inside_its_terminator_survives() -> None:
    # The nastiest split: the blank line that ends the frame arrives one byte at a
    # time, so a naive `endswith(b"\n\n")` check sees neither half as terminal.
    parser = SseParser()
    assert list(parser.feed(b'event: exit\ndata: {"offset": 5}\n')) == []
    frames = list(parser.feed(b"\n"))
    assert len(frames) == 1
    assert frames[0].event == "exit"


def test_one_read_carrying_several_frames_yields_all_of_them() -> None:
    parser = SseParser()
    blob = b'event: output\ndata: {"offset": 0}\n\nevent: output\ndata: {"offset": 2}\n\n'
    frames = list(parser.feed(blob))
    assert [f.data["offset"] for f in frames] == [0, 2]


def test_a_keepalive_comment_is_not_a_frame() -> None:
    # axum sends `:` comments every 15s. Treating one as a frame would inject an
    # unnamed event into the caller's stream on a timer.
    parser = SseParser()
    assert list(parser.feed(b": keepalive\n\n")) == []
    frames = list(parser.feed(b'event: output\ndata: {"offset": 0}\n\n'))
    assert len(frames) == 1


def test_crlf_line_endings_parse() -> None:
    # A proxy in the path may rewrite line endings, and picking one spelling would
    # work right up until it did not.
    parser = SseParser()
    frames = list(parser.feed(b'event: output\r\ndata: {"offset": 7}\r\n\r\n'))
    assert len(frames) == 1
    assert frames[0].data == {"offset": 7}


def test_only_one_leading_space_is_stripped_from_a_value() -> None:
    parser = SseParser()
    frames = list(parser.feed(b'event: output\ndata:  {"offset": 0}\n\n'))
    # The value is `' {"offset": 0}'` after stripping one space, which is still
    # valid JSON with leading whitespace — the point is that exactly one space was
    # removed, not that JSON tolerated it.
    assert len(frames) == 1


def test_a_frame_with_unparseable_json_is_dropped_not_raised() -> None:
    # The daemon degrades a serialization failure to `{}` rather than taking the
    # connection down; one bad frame must not end an otherwise live stream.
    parser = SseParser()
    assert list(parser.feed(b"event: output\ndata: not json\n\n")) == []
    frames = list(parser.feed(b'event: exit\ndata: {"offset": 1}\n\n'))
    assert len(frames) == 1


def test_multi_line_data_is_joined_with_newlines() -> None:
    parser = SseParser()
    frames = list(parser.feed(b'event: output\ndata: {"a":\ndata: 1}\n\n'))
    assert len(frames) == 1
    assert frames[0].data == {"a": 1}
