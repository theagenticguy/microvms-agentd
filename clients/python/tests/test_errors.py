"""The error taxonomy: every status the daemon chooses maps to its own type.

Each assertion here is a rule from `docs/PROTOCOL.md` that a defect bought. The
retryable/fatal split is the one a caller acts on, so it is asserted directly
rather than left implicit in the class hierarchy.
"""

from __future__ import annotations

import pytest

from microvms_agentd.errors import (
    AgentdError,
    AuthTokenMintError,
    Conflict,
    HttpError,
    NotBootstrapped,
    NotFound,
    ProtocolError,
    RequestTimeout,
    ServerError,
    StdinClosed,
    TooLarge,
    TransportError,
    Unauthorized,
    error_for_status,
)
from microvms_agentd.session import Session


@pytest.mark.parametrize(
    ("status", "expected"),
    [
        (400, ProtocolError),
        (401, Unauthorized),
        (404, NotFound),
        (408, RequestTimeout),
        (409, Conflict),
        (410, StdinClosed),
        (413, TooLarge),
        (503, NotBootstrapped),
        (500, ServerError),
    ],
)
def test_each_status_maps_to_its_own_type(status: int, expected: type[HttpError]) -> None:
    err = error_for_status(status, b"detail", method="GET", path="/v1/health")
    assert type(err) is expected
    assert err.status == status


def test_the_retryable_split_is_the_one_a_caller_acts_on() -> None:
    # 503 is "not yet bootstrapped", which resolves on its own in under a second.
    # 401 is a wrong credential and no amount of waiting fixes it. A client that
    # confuses these either spins forever or fails a launch that was fine.
    assert NotBootstrapped("").retryable is True
    assert Unauthorized("").retryable is False
    assert ProtocolError("").retryable is False
    assert NotFound("").retryable is False
    assert Conflict("").retryable is False
    assert TooLarge("").retryable is False
    assert StdinClosed("").retryable is False
    assert TransportError("").retryable is True
    assert AuthTokenMintError("").retryable is True


def test_400_is_never_reached_through_a_generic_4xx_fallback() -> None:
    # The daemon's stated contract is that a missing body key is 400 and never 404,
    # because clients map 404 onto FileNotFoundError. A fallback that collapsed the
    # 4xx range would reintroduce that phantom-missing-file defect.
    assert type(error_for_status(400, b"", method="GET", path="/")) is ProtocolError
    assert type(error_for_status(404, b"", method="GET", path="/")) is NotFound
    # An unmapped 4xx stays generic rather than being guessed at.
    assert type(error_for_status(418, b"", method="GET", path="/")) is HttpError


def test_every_error_is_catchable_as_one_base() -> None:
    for err in (NotBootstrapped(""), TransportError(""), AuthTokenMintError("")):
        assert isinstance(err, AgentdError)


def test_the_detail_and_body_travel_with_the_error() -> None:
    err = error_for_status(
        400, b"refused tar member ../escape: parent traversal", method="PUT", path="/v1/fs/tar"
    )
    assert "PUT /v1/fs/tar -> 400" in str(err)
    assert "parent traversal" in str(err)
    assert err.body.startswith(b"refused")


def test_a_503_from_the_daemon_surfaces_as_not_bootstrapped(daemon) -> None:
    from fake_daemon import Route

    daemon.on("GET", "/v1/exec/anything", Route(status=503, body=b""))
    with Session(endpoint=daemon.url, agent_token="t") as session, pytest.raises(NotBootstrapped):
        session.exec("anything").poll()


def test_an_absent_file_is_not_found_and_a_missing_key_is_a_protocol_error(daemon) -> None:
    from fake_daemon import Route

    daemon.on("GET", "/v1/fs/file", Route(status=404, body=b"no such file"))
    with Session(endpoint=daemon.url, agent_token="t") as session:
        with pytest.raises(NotFound):
            session.download_file("/tmp/absent")
        assert session.file_exists("/tmp/absent") is False

        daemon.on("GET", "/v1/fs/file", Route(status=400, body=b"path query parameter is required"))
        with pytest.raises(ProtocolError):
            session.download_file("/tmp/whatever")


def test_a_connection_to_nothing_is_a_transport_error() -> None:
    # Port 1 on loopback: nothing listens, so the connection is refused before any
    # status exists. Retryable, because it says nothing about the daemon's state.
    with Session(endpoint="http://127.0.0.1:1", agent_token="t", timeout=1.0) as session:
        with pytest.raises(TransportError) as caught:
            session.health()
        assert caught.value.retryable is True
