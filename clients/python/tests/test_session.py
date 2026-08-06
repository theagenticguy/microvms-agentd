"""The request shapes Session puts on the wire, checked field by field.

Not "does it work" but "does it send exactly what the daemon reads". A field name
that drifts here fails as a 400 at the far end of an AWS build cycle.
"""

from __future__ import annotations

import json
import tarfile
from pathlib import Path

import pytest

from fake_daemon import Route
from microvms_agentd.errors import Conflict, ExecTimeout, StdinClosed
from microvms_agentd.models import Phase
from microvms_agentd.session import Session


def body_of(recorded) -> dict:
    return json.loads(recorded.body)


def test_run_sends_argv_as_a_list_and_omits_every_default(daemon) -> None:
    # Omission is meaningful: an omitted cwd means the child inherits the image
    # WORKDIR, and sending "/" instead breaks prebuilt-image tasks. Same for
    # `shell` and `stdin`, which the daemon defaults to false.
    daemon.on("POST", "/v1/exec/start", Route(body=b'{"exec_id":"a","phase":"running"}'))
    with Session(endpoint=daemon.url, agent_token="t") as session:
        session.run(["/bin/echo", "hi"], exec_id="a")

    sent = body_of(daemon.calls("POST", "/v1/exec/start")[0])
    assert sent == {"exec_id": "a", "command": ["/bin/echo", "hi"]}


def test_a_bare_string_is_not_split_on_whitespace(daemon) -> None:
    # Splitting is where quoting bugs come from, and the daemon's contract is that
    # an argv array execs directly with no shell.
    daemon.on("POST", "/v1/exec/start", Route(body=b'{"exec_id":"b","phase":"running"}'))
    with Session(endpoint=daemon.url, agent_token="t") as session:
        session.run("/bin/ls -la", exec_id="b")
    assert body_of(daemon.calls("POST", "/v1/exec/start")[0])["command"] == ["/bin/ls -la"]


def test_shell_true_sends_a_single_element_command(daemon) -> None:
    daemon.on("POST", "/v1/exec/start", Route(body=b'{"exec_id":"c","phase":"running"}'))
    with Session(endpoint=daemon.url, agent_token="t") as session:
        session.run("echo a } echo b", shell=True, exec_id="c")
    sent = body_of(daemon.calls("POST", "/v1/exec/start")[0])
    assert sent["command"] == ["echo a } echo b"]
    assert sent["shell"] is True


def test_shell_true_with_an_argv_list_is_refused_locally(daemon) -> None:
    # `sh -c` takes the script as one argument; extra elements silently become $0
    # and $1 to that shell, which is a surprising place to lose an argument.
    with (
        Session(endpoint=daemon.url, agent_token="t") as session,
        pytest.raises(ValueError, match="single script string"),
    ):
        session.run(["echo", "a"], shell=True)


def test_every_optional_field_uses_the_daemons_own_name(daemon) -> None:
    daemon.on("POST", "/v1/exec/start", Route(body=b'{"exec_id":"d","phase":"running"}'))
    with Session(endpoint=daemon.url, agent_token="t") as session:
        session.run(
            ["/bin/true"],
            cwd="/work",
            env={"K": "V"},
            user=1000,
            group=1000,
            timeout_sec=12.5,
            stdin=True,
            exec_id="d",
        )
    assert body_of(daemon.calls("POST", "/v1/exec/start")[0]) == {
        "exec_id": "d",
        "command": ["/bin/true"],
        "cwd": "/work",
        "env": {"K": "V"},
        "user": 1000,
        "group": 1000,
        "timeout_sec": 12.5,
        "stdin": True,
    }


def test_an_omitted_exec_id_is_generated(daemon) -> None:
    daemon.on("POST", "/v1/exec/start", Route(body=b'{"exec_id":"gen","phase":"running"}'))
    with Session(endpoint=daemon.url, agent_token="t") as session:
        handle = session.run(["/bin/true"])
    sent = body_of(daemon.calls("POST", "/v1/exec/start")[0])
    assert sent["exec_id"], "an id is always minted client-side; it is the idempotency key"
    assert handle.exec_id == "gen", "and the daemon's echoed id wins"


def test_poll_parses_the_flattened_outcome(daemon) -> None:
    daemon.on(
        "GET",
        "/v1/exec/p1",
        Route(
            body=json.dumps(
                {
                    "exec_id": "p1",
                    "phase": "exited",
                    "exit_code": 0,
                    "signal": None,
                    "stdout": "live\n",
                    "stderr": "",
                    "truncated": True,
                    "writers_may_be_alive": True,
                }
            ).encode()
        ),
    )
    with Session(endpoint=daemon.url, agent_token="t") as session:
        result = session.exec("p1").poll()
    assert result.phase is Phase.EXITED
    assert result.done and result.ok
    assert result.truncated is True
    assert result.writers_may_be_alive is True


def test_a_running_exec_reports_no_output_rather_than_empty_output(daemon) -> None:
    # None, not "". A caller has to be able to tell "still running" from "produced
    # nothing", and an empty string cannot express the difference.
    daemon.on("GET", "/v1/exec/p2", Route(body=b'{"exec_id":"p2","phase":"running"}'))
    with Session(endpoint=daemon.url, agent_token="t") as session:
        result = session.exec("p2").poll()
    assert result.stdout is None
    assert result.done is False


def test_unknown_response_fields_are_kept_not_dropped(daemon) -> None:
    daemon.on(
        "GET",
        "/v1/exec/p3",
        Route(body=b'{"exec_id":"p3","phase":"exited","future_field":42}'),
    )
    with Session(endpoint=daemon.url, agent_token="t") as session:
        assert session.exec("p3").poll().extra == {"future_field": 42}


def test_wait_gives_up_without_touching_the_exec(daemon) -> None:
    daemon.on("GET", "/v1/exec/w1", Route(body=b'{"exec_id":"w1","phase":"running"}'))
    with (
        Session(endpoint=daemon.url, agent_token="t") as session,
        pytest.raises(ExecTimeout, match="re-polled"),
    ):
        session.exec("w1").wait(timeout=0.3, interval=0.05)
    # Only GETs. Polling is read-only, so a timeout leaves the record and its
    # output intact for another attempt.
    assert all(c.method == "GET" for c in daemon.requests)


def test_wait_and_ack_returns_the_ack_response_which_carries_the_output(daemon) -> None:
    # The sequencing is the point: an ack *takes* the output, so a poll issued after
    # one reports phase=acked with nothing in it. Returning the poll would be a
    # silent empty-output bug.
    daemon.on("GET", "/v1/exec/a1", Route(body=b'{"exec_id":"a1","phase":"exited"}'))
    daemon.on(
        "POST",
        "/v1/exec/a1/ack",
        Route(body=b'{"exec_id":"a1","phase":"acked","exit_code":0,"stdout":"payload"}'),
    )
    with Session(endpoint=daemon.url, agent_token="t") as session:
        result = session.exec("a1").wait_and_ack(timeout=5)
    assert result.stdout == "payload"
    assert result.phase is Phase.ACKED


def test_an_already_acked_exec_is_not_acked_twice(daemon) -> None:
    # A second ack is 409. Waiting on an exec that is already acked and then acking
    # it again would turn a completed command into an error.
    daemon.on("GET", "/v1/exec/a2", Route(body=b'{"exec_id":"a2","phase":"acked"}'))
    with Session(endpoint=daemon.url, agent_token="t") as session:
        result = session.exec("a2").wait_and_ack(timeout=5)
    assert result.phase is Phase.ACKED
    assert daemon.calls("POST", "/v1/exec/a2/ack") == []


def test_a_second_ack_surfaces_as_a_conflict(daemon) -> None:
    daemon.on("POST", "/v1/exec/a3/ack", Route(status=409, body=b"already_acked"))
    with Session(endpoint=daemon.url, agent_token="t") as session, pytest.raises(Conflict):
        session.exec("a3").ack()


def test_stdin_is_base64_and_eof_rides_the_same_request(daemon) -> None:
    # One round trip, because two would leave a window where the child has the bytes
    # but not the EOF that says the input is complete.
    daemon.on("POST", "/v1/exec/s1/stdin", Route(body=b'{"exec_id":"s1","written":5,"eof":true}'))
    with Session(endpoint=daemon.url, agent_token="t") as session:
        ack = session.exec("s1").write_stdin(b"hello", eof=True)
    assert body_of(daemon.calls("POST", "/v1/exec/s1/stdin")[0]) == {
        "data_b64": "aGVsbG8=",
        "signal": "eof",
    }
    assert (ack.written, ack.eof) == (5, True)


def test_close_stdin_sends_eof_with_no_data(daemon) -> None:
    daemon.on("POST", "/v1/exec/s2/stdin", Route(body=b'{"exec_id":"s2","written":0,"eof":true}'))
    with Session(endpoint=daemon.url, agent_token="t") as session:
        session.exec("s2").close_stdin()
    assert body_of(daemon.calls("POST", "/v1/exec/s2/stdin")[0]) == {"signal": "eof"}


def test_stdin_on_an_exec_that_never_asked_for_it_is_a_conflict(daemon) -> None:
    # 409 not 400: the request is well-formed, the exec cannot accept it, and the fix
    # is at start time.
    daemon.on("POST", "/v1/exec/s3/stdin", Route(status=409, body=b"stdin_not_requested"))
    with Session(endpoint=daemon.url, agent_token="t") as session, pytest.raises(Conflict):
        session.exec("s3").write_stdin(b"x")


def test_stdin_after_eof_is_gone_not_a_conflict(daemon) -> None:
    # 410: the pipe no longer exists, and a retry can never succeed. Distinct from
    # 409, which is fixable at start time.
    daemon.on("POST", "/v1/exec/s4/stdin", Route(status=410, body=b"stdin_closed"))
    with (
        Session(endpoint=daemon.url, agent_token="t") as session,
        pytest.raises(StdinClosed) as caught,
    ):
        session.exec("s4").write_stdin(b"x")
    assert caught.value.retryable is False


def test_kill_reports_whether_anything_was_signaled(daemon) -> None:
    daemon.on("POST", "/v1/exec/k1/kill", Route(body=b'{"exec_id":"k1","killed":false}'))
    with Session(endpoint=daemon.url, agent_token="t") as session:
        assert session.kill("k1") is False


def test_file_upload_sends_the_mode_as_an_octal_string(daemon) -> None:
    # A string because the daemon parses it as octal: "644" and "0644" mean the same
    # thing, and an int would be stringified as decimal 644.
    daemon.on("PUT", "/v1/fs/file", Route(status=204))
    with Session(endpoint=daemon.url, agent_token="t") as session:
        session.upload_file("/tmp/x", b"bytes", mode="0644")
    call = daemon.calls("PUT", "/v1/fs/file")[0]
    assert call.query == {"path": ["/tmp/x"], "mode": ["0644"]}
    assert call.body == b"bytes"


def test_a_str_payload_is_utf8_encoded(daemon) -> None:
    daemon.on("PUT", "/v1/fs/file", Route(status=204))
    with Session(endpoint=daemon.url, agent_token="t") as session:
        session.upload_file("/tmp/x", "héllo")
    assert daemon.calls("PUT", "/v1/fs/file")[0].body == "héllo".encode()


def test_dir_upload_packs_a_tar_whose_members_are_relative(daemon) -> None:
    # Relative members are what the daemon's confined extraction accepts; an absolute
    # or parent-traversing member is refused with 400.
    import io

    daemon.on("PUT", "/v1/fs/tar", Route(status=204))
    with Session(endpoint=daemon.url, agent_token="t") as session:
        root = Path(daemon.state.setdefault("tmp", _tmpdir()))
        (root / "sub").mkdir()
        (root / "a.txt").write_text("payload")
        (root / "sub" / "b.txt").write_text("deep")
        session.upload_dir(root, "/tmp/dest")

    archive = daemon.calls("PUT", "/v1/fs/tar")[0].body
    with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
        names = sorted(tar.getnames())
    assert names == ["a.txt", "sub", "sub/b.txt"]
    assert daemon.calls("PUT", "/v1/fs/tar")[0].query["path"] == ["/tmp/dest"]


def test_a_symlink_is_packed_as_a_link_not_followed(daemon) -> None:
    # Following it would silently change what a round trip means: the daemon
    # preserves in-tree links on extraction, and this is the producing half.
    import io

    daemon.on("PUT", "/v1/fs/tar", Route(status=204))
    with Session(endpoint=daemon.url, agent_token="t") as session:
        root = Path(_tmpdir())
        (root / "a.txt").write_text("payload")
        (root / "link").symlink_to("a.txt")
        session.upload_dir(root, "/tmp/dest")

    with tarfile.open(fileobj=io.BytesIO(daemon.calls("PUT", "/v1/fs/tar")[0].body)) as tar:
        member = tar.getmember("link")
    assert member.issym()
    assert member.linkname == "a.txt"


def test_dir_download_round_trips_through_the_data_filter(daemon, tmp_path) -> None:
    import io

    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w") as tar:
        info = tarfile.TarInfo("f.txt")
        payload = b"round trip"
        info.size = len(payload)
        tar.addfile(info, io.BytesIO(payload))

    daemon.on("GET", "/v1/fs/tar", Route(body=buffer.getvalue(), content_type="application/x-tar"))
    target = tmp_path / "out"
    with Session(endpoint=daemon.url, agent_token="t") as session:
        session.download_dir("/tmp/tree", target)
    assert (target / "f.txt").read_bytes() == b"round trip"


def _tmpdir() -> str:
    import tempfile

    return tempfile.mkdtemp()
