"""`Session`: one MicroVM's control API, with the proxy auth handled for you.

A Session is cheap and re-creatable. It holds no server-side state of its own —
every exec record, every file, and the bootstrap token live in the VM — so
rebuilding one from a MicroVM id, an endpoint, and an agent token reattaches to
everything a previous process was doing.
"""

from __future__ import annotations

import io
import secrets
import tarfile
import time
from collections.abc import Mapping
from pathlib import Path
from typing import Any

import httpx

from .errors import AgentdError, NotFound
from .exec_handle import ExecHandle
from .models import ExecResult, Health
from .transport import DEFAULT_AGENT_PORT, MicrovmClient, ProxyAuth, Transport


class Session:
    """The control API of one running MicroVM.

    Usable as a context manager, which closes the pooled HTTP client.
    """

    def __init__(
        self,
        *,
        endpoint: str,
        agent_token: str,
        microvm_id: str | None = None,
        microvm_client: MicrovmClient | None = None,
        port: int = DEFAULT_AGENT_PORT,
        timeout: float = 60.0,
        http_client: httpx.Client | None = None,
    ) -> None:
        """
        `microvm_client` and `microvm_id` are what enable proxy auth. Both omitted
        means no proxy headers are sent at all, which is the shape for talking to a
        daemon directly — a local binary, a test server, or a VM reached over a
        tunnel. Requiring boto3 for that case would make the library untestable
        without AWS.
        """
        proxy_auth: ProxyAuth | None = None
        if microvm_client is not None and microvm_id is not None:
            proxy_auth = ProxyAuth(microvm_client, microvm_id, port=port)
        self.microvm_id = microvm_id
        self.port = port
        self.timeout = timeout
        self.transport = Transport(
            endpoint,
            agent_token,
            proxy_auth=proxy_auth,
            timeout=timeout,
            client=http_client,
        )

    def __enter__(self) -> Session:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

    def close(self) -> None:
        self.transport.close()

    @property
    def endpoint(self) -> str:
        return self.transport.base_url

    @property
    def agent_token(self) -> str:
        return self.transport.agent_token

    def rebind(self, endpoint: str) -> None:
        """Points the session at a new endpoint and drops the cached proxy token.

        Called after a resume. The measured behavior is that the endpoint URL does
        not change across suspend/resume, so this is usually a no-op on the URL —
        but the token drop is not: a token minted against the pre-suspend instance
        may no longer validate, and that rejection reads exactly like a dead daemon.
        """
        self.transport.base_url = endpoint if endpoint.startswith("http") else f"https://{endpoint}"
        if self.transport.proxy_auth is not None:
            self.transport.proxy_auth.invalidate()

    # -- health ------------------------------------------------------------

    def health(self) -> Health:
        """Unauthenticated liveness. `bootstrapped` is the useful field.

        Reachable through the endpoint at all implies bootstrapped in practice —
        the platform forwards no external traffic until the run hook returns 200 —
        but a caller inside the VM, or one talking to the daemon directly, can
        observe the pre-bootstrap state.
        """
        response = self.transport.send("GET", "/v1/health", token=None)
        return Health.from_json(response.json())

    def wait_until_ready(self, timeout: float = 120.0, interval: float = 2.0) -> Health:
        """Polls health until the daemon reports bootstrapped.

        Connection errors are expected here rather than exceptional: a VM that has
        just reached RUNNING commonly refuses a connection or two before the proxy
        path is wired up, and a mint failure is retryable by construction.
        """
        deadline = time.monotonic() + timeout
        last: Exception | None = None
        while time.monotonic() < deadline:
            try:
                health = self.health()
                if health.bootstrapped:
                    return health
            except AgentdError as exc:
                if not exc.retryable:
                    raise
                last = exc
            time.sleep(interval)
        detail = f" (last error: {last})" if last else ""
        raise TimeoutError(f"daemon was not bootstrapped within {timeout:.0f}s{detail}")

    # -- exec --------------------------------------------------------------

    def run(
        self,
        command: str | list[str],
        *,
        shell: bool = False,
        cwd: str | None = None,
        env: Mapping[str, str] | None = None,
        user: int | None = None,
        group: int | None = None,
        timeout_sec: float | None = None,
        stdin: bool = False,
        exec_id: str | None = None,
    ) -> ExecHandle:
        """Starts a command and returns its handle. Does not wait.

        `command` is argv, or a single script string when `shell=True`. A bare
        string with `shell=False` is wrapped into a one-element argv rather than
        split on whitespace: splitting is where quoting bugs come from, and the
        daemon's contract is that an argv array execs directly with no shell.

        `cwd` omitted means the child inherits the daemon's working directory,
        which is the image WORKDIR. Passing `/` explicitly is not the same thing
        and breaks any prebuilt-image task that expects its own workdir.

        `exec_id` is the idempotency key. Generated when omitted, but a caller that
        wants a retry to be safe across its own restart must supply a stable one:
        the daemon returns success for a known id without spawning a second child,
        and that guarantee is only reachable if the caller can name the id again.
        """
        argv = [command] if isinstance(command, str) else list(command)
        if shell and len(argv) != 1:
            # `shell: true` wraps in `sh -c` with the command as one argument. More
            # than one element would silently become `$0`/`$1` to that shell, which
            # is a surprising place to lose an argument.
            raise ValueError("shell=True takes a single script string, not an argv list")

        body: dict[str, Any] = {
            "exec_id": exec_id or f"x-{secrets.token_hex(8)}",
            "command": argv,
        }
        if shell:
            body["shell"] = True
        if cwd is not None:
            body["cwd"] = cwd
        if env:
            body["env"] = dict(env)
        if user is not None:
            body["user"] = user
        if group is not None:
            body["group"] = group
        if timeout_sec is not None:
            body["timeout_sec"] = timeout_sec
        if stdin:
            body["stdin"] = True

        response = self.transport.send("POST", "/v1/exec/start", json=body)
        return ExecHandle(self, str(response.json().get("exec_id", body["exec_id"])))

    def exec(self, exec_id: str) -> ExecHandle:
        """A handle for an exec started earlier, possibly by another process."""
        return ExecHandle(self, exec_id)

    def run_sync(
        self,
        command: str | list[str],
        *,
        timeout: float = 300.0,
        **kwargs: Any,
    ) -> ExecResult:
        """Start, wait, ack. The one-shot shape, for when output is all you want."""
        return self.run(command, **kwargs).wait_and_ack(timeout=timeout)

    def kill(self, exec_id: str) -> bool:
        """Signals an exec's whole process group. Returns whether anything was signaled."""
        return ExecHandle(self, exec_id).kill()

    # -- file transfer -----------------------------------------------------

    def upload_file(self, path: str, data: bytes | str, *, mode: str | None = None) -> None:
        """Writes one file, creating parents. `mode` is octal as a string.

        Octal as a string because that is what the daemon parses: `"644"` and
        `"0644"` both mean the same mode, and an int would be read as decimal 644
        by anything that stringifies it.
        """
        params: dict[str, Any] = {"path": path}
        if mode is not None:
            params["mode"] = mode
        payload = data.encode("utf-8") if isinstance(data, str) else data
        self.transport.send("PUT", "/v1/fs/file", params=params, content=payload)

    def download_file(self, path: str) -> bytes:
        """Reads one file. `NotFound` only when the path is genuinely absent —
        a missing `path` key is 400 and a directory is 400."""
        return self.transport.send("GET", "/v1/fs/file", params={"path": path}).content

    def file_exists(self, path: str) -> bool:
        try:
            self.download_file(path)
        except NotFound:
            return False
        return True

    def upload_dir(self, local: str | Path, remote: str) -> None:
        """Packs a local tree and extracts it under `remote`, which must be absolute.

        Symlinks are packed as links, matching what the daemon's extraction accepts:
        in-tree links survive, absolute link targets are refused with 400. Following
        them here would silently change what a round trip means.
        """
        root = Path(local)
        if not root.is_dir():
            raise NotADirectoryError(f"{root} is not a directory")
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode="w") as tar:
            for child in sorted(root.iterdir()):
                tar.add(child, arcname=child.name, recursive=True)
        self.transport.send("PUT", "/v1/fs/tar", params={"path": remote}, content=buffer.getvalue())

    def download_dir(self, remote: str, local: str | Path) -> None:
        """Downloads a remote tree and extracts it into `local`.

        Extracted with the `data` filter, which is the same contract the daemon
        enforces on upload. Trusting an archive on the way out because the daemon
        checks it on the way in would be the wrong direction of trust: the archive
        describes the VM's filesystem, and the VM is where untrusted work runs.
        """
        archive = self.download_tar(remote)
        target = Path(local)
        target.mkdir(parents=True, exist_ok=True)
        with tarfile.open(fileobj=io.BytesIO(archive), mode="r:*") as tar:
            tar.extractall(target, filter="data")

    def download_tar(self, remote: str) -> bytes:
        """The raw tar bytes of a remote tree, for a caller doing its own unpacking."""
        return self.transport.send("GET", "/v1/fs/tar", params={"path": remote}).content

    def upload_tar(self, remote: str, archive: bytes) -> None:
        """Extracts pre-built tar bytes under `remote`."""
        self.transport.send("PUT", "/v1/fs/tar", params={"path": remote}, content=archive)
