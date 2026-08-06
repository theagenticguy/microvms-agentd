"""Proxy token minting: both headers, cached, refreshed, and retryable on failure.

Proxy tokens expire at 60 minutes — a hard service ceiling — so a long agent run
crosses it. That makes minting part of the request path, and a mint failure at
minute 30 something that must not kill an otherwise healthy trial.
"""

from __future__ import annotations

import pytest

from fake_daemon import FakeMicrovmClient, Route
from microvms_agentd.errors import AuthTokenMintError
from microvms_agentd.session import Session
from microvms_agentd.transport import MAX_TOKEN_MINUTES, ProxyAuth


class FakeClock:
    def __init__(self) -> None:
        self.now = 1000.0

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


def test_both_proxy_headers_are_sent(daemon) -> None:
    # X-aws-proxy-port is not optional: the proxy needs to know which of the
    # token's allowed ports this request targets, and omitting it is a rejection
    # that reads like a bad token.
    mv = FakeMicrovmClient()
    daemon.on("GET", "/v1/health", Route(body=b'{"version":"1","bootstrapped":true}'))
    with Session(
        endpoint=daemon.url, agent_token="t", microvm_id="mvm-1", microvm_client=mv, port=9000
    ) as session:
        session.health()

    headers = daemon.calls("GET", "/v1/health")[0].headers
    assert headers["x-aws-proxy-auth"] == "token-0"
    assert headers["x-aws-proxy-port"] == "9000"


def test_the_token_is_read_from_the_header_map_not_a_bare_string() -> None:
    mv = FakeMicrovmClient()
    auth = ProxyAuth(mv, "mvm-1")
    assert auth.headers()["X-aws-proxy-auth"] == "token-0"
    assert mv.calls[0]["expirationInMinutes"] == MAX_TOKEN_MINUTES
    assert mv.calls[0]["allowedPorts"] == [{"port": 9000}]
    assert mv.calls[0]["microvmIdentifier"] == "mvm-1"


def test_a_cached_token_is_reused_rather_than_reminted_per_request() -> None:
    mv = FakeMicrovmClient()
    auth = ProxyAuth(mv, "mvm-1", clock=FakeClock())
    for _ in range(5):
        auth.headers()
    assert auth.mint_count == 1, "one mint for five requests"


def test_the_token_refreshes_well_before_the_60_minute_ceiling() -> None:
    clock = FakeClock()
    auth = ProxyAuth(FakeMicrovmClient(), "mvm-1", clock=clock)
    assert auth.headers()["X-aws-proxy-auth"] == "token-0"

    clock.advance(29 * 60)
    assert auth.headers()["X-aws-proxy-auth"] == "token-0", "still fresh at 29 minutes"

    # Refresh at 30, not 59: a token minted at 59:59 would expire between building
    # the headers and the proxy validating them.
    clock.advance(2 * 60)
    assert auth.headers()["X-aws-proxy-auth"] == "token-1"
    assert auth.mint_count == 2


def test_a_mint_failure_is_retryable_rather_than_fatal() -> None:
    # A throttle from the control plane at minute 30 of a two-hour run must be
    # survivable. The type carries `retryable=True` so a caller's retry loop does
    # not have to special-case boto3's exception family.
    mv = FakeMicrovmClient(fail_times=1)
    auth = ProxyAuth(mv, "mvm-1", clock=FakeClock())

    with pytest.raises(AuthTokenMintError) as caught:
        auth.headers()
    assert caught.value.retryable is True

    assert auth.headers()["X-aws-proxy-auth"] == "token-1", "the retry succeeds"


def test_invalidate_forces_a_fresh_mint_after_a_resume() -> None:
    clock = FakeClock()
    auth = ProxyAuth(FakeMicrovmClient(), "mvm-1", clock=clock)
    assert auth.headers()["X-aws-proxy-auth"] == "token-0"

    auth.invalidate()
    assert auth.headers()["X-aws-proxy-auth"] == "token-1"


def test_rebind_after_resume_drops_the_token_and_can_change_the_endpoint(daemon) -> None:
    mv = FakeMicrovmClient()
    daemon.on("GET", "/v1/health", Route(body=b'{"version":"1","bootstrapped":true}'))
    session = Session(endpoint=daemon.url, agent_token="t", microvm_id="mvm-1", microvm_client=mv)
    session.health()
    assert daemon.calls("GET", "/v1/health")[0].headers["x-aws-proxy-auth"] == "token-0"

    # Measured behavior is that the endpoint does not change across suspend/resume,
    # so passing the same one is the normal case. The token drop is the part that
    # matters.
    session.rebind(daemon.url)
    session.health()
    assert daemon.calls("GET", "/v1/health")[1].headers["x-aws-proxy-auth"] == "token-1"
    session.close()


def test_no_microvm_client_means_no_proxy_headers(daemon) -> None:
    # Talking to a daemon directly — a local binary, a tunnel, a test server — must
    # not require boto3 or an AWS credential. Requiring it would make the library
    # untestable without AWS, which is the whole reason this path exists.
    daemon.on("GET", "/v1/health", Route(body=b'{"version":"1","bootstrapped":true}'))
    with Session(endpoint=daemon.url, agent_token="t") as session:
        session.health()
    headers = daemon.calls("GET", "/v1/health")[0].headers
    assert "x-aws-proxy-auth" not in headers


def test_the_health_route_is_called_with_no_authorization_header(daemon) -> None:
    daemon.on("GET", "/v1/health", Route(body=b'{"version":"1","bootstrapped":false}'))
    with Session(endpoint=daemon.url, agent_token="secret") as session:
        assert session.health().bootstrapped is False
    assert "authorization" not in daemon.calls("GET", "/v1/health")[0].headers


def test_a_non_ascii_agent_token_reaches_the_wire_as_bytes(daemon) -> None:
    # The daemon's stated property is that it compares header bytes without
    # decoding them, and that is only observable if a client can put non-ASCII on
    # the wire. httpx refuses a str header that is not ASCII, so the bearer value
    # is encoded to bytes before it is handed over.
    daemon.on("GET", "/v1/exec/x", Route(status=401, body=b""))
    with Session(endpoint=daemon.url, agent_token="tökén") as session:
        from microvms_agentd.errors import Unauthorized

        with pytest.raises(Unauthorized):
            session.exec("x").poll()
    sent = daemon.calls("GET", "/v1/exec/x")[0].headers["authorization"]
    assert sent.encode("latin-1").decode("utf-8") == "Bearer tökén"
