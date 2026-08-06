"""HTTP transport: proxy auth minting, both required headers, typed statuses.

Two headers, not one. `X-aws-proxy-auth` carries a JWE scoped to a MicroVM id and
a port set; `X-aws-proxy-port` names which of that token's allowed ports this
request targets. Omitting the second is a rejection that reads like a bad token.
Both measured 2026-08-05; see `docs/PLATFORM.md`.
"""

from __future__ import annotations

import time
from collections.abc import Iterator, Mapping
from contextlib import contextmanager
from typing import Any, Protocol

import httpx

from .errors import AuthTokenMintError, TransportError, error_for_status

#: The port the daemon listens on in the images this repo builds (`AGENTD_PORT`).
DEFAULT_AGENT_PORT = 9000

#: Ceiling the service enforces on a proxy token. Not a choice.
MAX_TOKEN_MINUTES = 60

#: When to re-mint. Half the ceiling, so a request in flight when the clock rolls
#: over is still holding a token with ~30 minutes left. Refreshing at 59 minutes
#: would put the expiry inside the window between building headers and the proxy
#: validating them.
DEFAULT_REFRESH_AFTER_SEC = 30 * 60


class MicrovmClient(Protocol):
    """The one boto3 method the transport needs, so tests need no AWS.

    Narrow on purpose: a fake in a test implements one method, and the Session is
    not coupled to a boto3 client it barely uses.
    """

    def create_microvm_auth_token(self, **kwargs: Any) -> Mapping[str, Any]: ...


class ProxyAuth:
    """Mints and caches the endpoint proxy token for one MicroVM and one port.

    Minting sits inside the request path rather than at construction because the
    60-minute ceiling is shorter than a long agent run. A failure here is
    `AuthTokenMintError`, which is retryable: a throttle from the control plane at
    minute 30 must not kill a healthy trial.
    """

    def __init__(
        self,
        client: MicrovmClient,
        microvm_id: str,
        *,
        port: int = DEFAULT_AGENT_PORT,
        refresh_after_sec: float = DEFAULT_REFRESH_AFTER_SEC,
        clock: Any = time.monotonic,
    ) -> None:
        self._client = client
        self._microvm_id = microvm_id
        self._port = port
        self._refresh_after = refresh_after_sec
        self._clock = clock
        self._token: str | None = None
        self._minted_at = 0.0
        self.mint_count = 0

    def invalidate(self) -> None:
        """Drops the cached token so the next request mints a fresh one.

        Called after a resume. The measured behavior is that a resumed VM keeps its
        endpoint URL and its bootstrap state, but a token minted against the
        pre-suspend instance is not guaranteed to survive, and a stale-token
        rejection there reads exactly like a daemon that died.
        """
        self._token = None

    def headers(self) -> dict[str, str]:
        return {
            "X-aws-proxy-auth": self._value(),
            "X-aws-proxy-port": str(self._port),
        }

    def _value(self) -> str:
        if self._token is not None and self._clock() - self._minted_at <= self._refresh_after:
            return self._token
        try:
            response = self._client.create_microvm_auth_token(
                microvmIdentifier=self._microvm_id,
                expirationInMinutes=MAX_TOKEN_MINUTES,
                allowedPorts=[{"port": self._port}],
            )
            # A map of header name to value, not a bare string: the API is shaped
            # for schemes needing more than one header.
            token = response["authToken"]["X-aws-proxy-auth"]
        except Exception as exc:
            raise AuthTokenMintError(f"could not mint a proxy auth token: {exc}") from exc
        self._token = str(token)
        self._minted_at = self._clock()
        self.mint_count += 1
        return self._token


class Transport:
    """One pooled httpx client plus the headers every request needs.

    Pooled rather than a client per request: the daemon drains a bounded prefix of
    a rejected body specifically so pooled connections keep working, and throwing
    the pool away per request discards that.
    """

    #: Sentinel meaning "use the session's agent token". `None` is a real value
    #: here — it means send no Authorization header at all, which is how the health
    #: and hook routes are exercised — so the default cannot be `None`.
    DEFAULT_TOKEN = object()

    def __init__(
        self,
        base_url: str,
        agent_token: str,
        *,
        proxy_auth: ProxyAuth | None = None,
        timeout: float = 60.0,
        client: httpx.Client | None = None,
    ) -> None:
        self.base_url = base_url if base_url.startswith("http") else f"https://{base_url}"
        self.agent_token = agent_token
        self.proxy_auth = proxy_auth
        self._timeout = timeout
        self._client = client or httpx.Client(timeout=timeout, verify=True)
        self._owns_client = client is None

    def close(self) -> None:
        if self._owns_client:
            self._client.close()

    def headers(self, token: Any = DEFAULT_TOKEN) -> dict[str, Any]:
        headers: dict[str, Any] = {}
        if self.proxy_auth is not None:
            headers.update(self.proxy_auth.headers())
        bearer = self.agent_token if token is self.DEFAULT_TOKEN else token
        if bearer is not None:
            # Bytes, not str. httpx encodes a str header as ASCII and refuses
            # anything else, which would make a non-ASCII token unsendable from
            # this library — and the daemon's stated property is that it compares
            # header bytes without decoding them, which is only testable if a
            # client can put arbitrary bytes on the wire.
            value = bearer.encode("utf-8") if isinstance(bearer, str) else bearer
            headers["Authorization"] = b"Bearer " + value
        return headers

    def request(
        self,
        method: str,
        path: str,
        *,
        token: Any = DEFAULT_TOKEN,
        json: Any = None,
        content: bytes | None = None,
        params: Mapping[str, Any] | None = None,
        timeout: float | None = None,
    ) -> httpx.Response:
        """Sends one request and returns the raw response, whatever its status.

        Deliberately does not raise on a status: a conformance suite asserts on
        401 and 409 as expected outcomes, and a client that could only reach them
        through exceptions would be a client that cannot test the protocol.
        `send()` is the raising wrapper.
        """
        try:
            return self._client.request(
                method,
                f"{self.base_url}{path}",
                headers=self.headers(token),
                json=json,
                content=content,
                params=dict(params) if params else None,
                timeout=timeout if timeout is not None else self._timeout,
            )
        except httpx.HTTPError as exc:
            raise TransportError(f"{method} {path} failed on the wire: {exc}") from exc

    def send(
        self,
        method: str,
        path: str,
        *,
        token: Any = DEFAULT_TOKEN,
        json: Any = None,
        content: bytes | None = None,
        params: Mapping[str, Any] | None = None,
        timeout: float | None = None,
    ) -> httpx.Response:
        """`request`, raising the typed error for any non-2xx status."""
        response = self.request(
            method,
            path,
            token=token,
            json=json,
            content=content,
            params=params,
            timeout=timeout,
        )
        if response.status_code >= 400:
            raise error_for_status(response.status_code, response.content, method=method, path=path)
        return response

    @contextmanager
    def stream(
        self,
        method: str,
        path: str,
        *,
        token: Any = DEFAULT_TOKEN,
        params: Mapping[str, Any] | None = None,
        timeout: float | httpx.Timeout | None = None,
    ) -> Iterator[httpx.Response]:
        """Opens a streaming response, raising the typed error for a non-2xx status.

        The status is read before any body byte, so a 404 on an unknown exec id
        surfaces as `NotFound` rather than as an empty stream.

        The caller supplies the timeout because a stream's is not a request's: an
        SSE body is idle by design between chunks, so the useful bound is how long
        silence is allowed to last rather than how long the whole body may take.
        """
        try:
            with self._client.stream(
                method,
                f"{self.base_url}{path}",
                headers=self.headers(token),
                params=dict(params) if params else None,
                timeout=timeout if timeout is not None else self._timeout,
            ) as response:
                if response.status_code >= 400:
                    response.read()
                    raise error_for_status(
                        response.status_code, response.content, method=method, path=path
                    )
                yield response
        except httpx.HTTPError as exc:
            raise TransportError(f"{method} {path} stream failed: {exc}") from exc
