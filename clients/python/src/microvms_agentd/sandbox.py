"""The AWS side: build an image, launch a MicroVM, hand back a `Session`.

Every function here is lifted from `conformance/run.py` and
`conformance/probe_suspend_resume.py`, which have been run against real AWS. The
traps are inlined as comments because each one cost a full build-and-run cycle to
find, and none of them is discoverable from the API shape.

boto3 is imported lazily so that `Session` — the part with no AWS in it — stays
importable and testable without credentials or the SDK.
"""

from __future__ import annotations

import contextlib
import io
import json
import secrets
import time
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .session import Session
from .transport import DEFAULT_AGENT_PORT

SERVICE = "lambda-microvms"

#: States the image API uses for "built and usable". More than one because the
#: service has answered differently across API versions and a hard equality check
#: on a single spelling is how a working build looks like a stalled one.
IMAGE_READY_STATES = frozenset({"CREATED", "ACTIVE", "AVAILABLE"})

#: Terminal states. Reaching any of them *before* RUNNING means the VM died during
#: startup, which for a hook-serving daemon almost always means a lifecycle hook
#: failed — and `stateReason` is where the answer is.
TERMINAL_STATES = frozenset({"TERMINATED", "TERMINATING", "SUSPENDED", "SUSPENDING"})

DEFAULT_BASE_IMAGE = "al2023-1"
DEFAULT_MEMORY_MIB = 1024
DEFAULT_HOOK_TIMEOUT_SEC = 30
DEFAULT_IMAGE_BUILD_TIMEOUT_SEC = 45 * 60
#: How long an image may sit in CREATING before we probe for the stalled-build
#: signature. Long enough that a genuinely slow build is not accused, short enough
#: that a wedged one does not burn the full 45 minutes in silence.
STALL_GRACE_SEC = 240
#: Gap between MicroVM state polls. A launch takes tens of seconds, so a tighter
#: interval only spends control-plane quota. Overridable per Sandbox so a test does
#: not have to sleep through it.
DEFAULT_POLL_INTERVAL_SEC = 5.0


def ingress_connector_arn(region: str) -> str:
    """The Lambda-managed connector that lets the endpoint proxy reach the VM.

    An ARN, not a name: the bare string `ALL_INGRESS` is rejected with "Malformed
    network connector ARN". Egress uses the same shape with `INTERNET_EGRESS`, and
    omitting egress is how you get a VM with no outbound network — which is the
    right default for a daemon that needs none.
    """
    return f"arn:aws:lambda:{region}:aws:network-connector:aws-network-connector:ALL_INGRESS"


def base_image_arn(region: str, name: str = DEFAULT_BASE_IMAGE) -> str:
    return f"arn:aws:lambda:{region}:aws:microvm-image:{name}"


def default_dockerfile(*, port: int = DEFAULT_AGENT_PORT, workdir: str | None = None) -> str:
    """A Dockerfile that makes the daemon the container CMD.

    `ENTRYPOINT []` plus `CMD ["/agentd"]` is the deployment invariant the trust
    boundary rests on: it is what guarantees no task workload runs before the
    platform's run hook lands. It is also what makes an omitted `cwd` inherit the
    image WORKDIR, since the daemon's own cwd is the image's.

    Note the invariant is *unenforced* — a base image that starts its own
    background process before bootstrap breaks it, and enforcing that belongs to
    whoever builds the image. See `docs/PROTOCOL.md`, "Trust boundary".
    """
    lines = [
        "FROM public.ecr.aws/amazonlinux/amazonlinux:2023-minimal",
        "COPY agentd /agentd",
        "RUN chmod 0755 /agentd",
    ]
    if workdir:
        lines += [f"RUN mkdir -p {workdir}", f"WORKDIR {workdir}"]
    lines += [
        f"ENV AGENTD_PORT={port}",
        "ENV AGENTD_LOG=info",
        f"EXPOSE {port}",
        "ENTRYPOINT []",
        'CMD ["/agentd"]',
        "",
    ]
    return "\n".join(lines)


def build_artifact(binary: str | Path, dockerfile: str | None = None) -> bytes:
    """Zips the daemon binary with a Dockerfile, which is what the image build takes."""
    path = Path(binary)
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("Dockerfile", dockerfile or default_dockerfile())
        info = zipfile.ZipInfo("agentd")
        # The execute bit has to be set in the zip entry: a build that copies a
        # non-executable binary produces an image whose CMD fails, and the failure
        # surfaces as a run-hook timeout rather than as anything about permissions.
        info.external_attr = 0o755 << 16
        archive.writestr(info, path.read_bytes())
    return buffer.getvalue()


def default_hooks(
    port: int = DEFAULT_AGENT_PORT, timeout: int = DEFAULT_HOOK_TIMEOUT_SEC
) -> dict[str, Any]:
    """Every hook the daemon serves, enabled.

    `ready` and `validate` are image-*build* hooks: the build calls them to decide
    whether the snapshot it just produced is usable, before any instance exists and
    therefore before any token has been delivered. Gating them on bootstrap state
    fails the build rather than the run, which is a confusing place to discover the
    mistake.
    """
    return {
        "port": port,
        "microvmImageHooks": {
            "ready": "ENABLED",
            "readyTimeoutInSeconds": timeout,
            "validate": "ENABLED",
            "validateTimeoutInSeconds": timeout,
        },
        "microvmHooks": {
            "run": "ENABLED",
            "runTimeoutInSeconds": timeout,
            "suspend": "ENABLED",
            "suspendTimeoutInSeconds": timeout,
            "resume": "ENABLED",
            "resumeTimeoutInSeconds": timeout,
            "terminate": "ENABLED",
            "terminateTimeoutInSeconds": timeout,
        },
    }


@dataclass
class Image:
    """A built MicroVM image, and the log group the service created alongside it."""

    identifier: str
    name: str

    @property
    def build_log_group(self) -> str:
        """`/aws/lambda-microvms/<image-name>`, not `/aws/lambda/microvms/*`.

        The service creates this itself, so a Terraform stack never owns it and
        `terraform destroy` leaves it behind — "the stack destroyed cleanly" is not
        "the account is clean". Delete it in teardown. An IAM policy granting the
        wrong prefix also produces builds with no logs at all, and every failure
        then reports `reason=unknown`, which reads as the service failing to
        populate `stateReason`.
        """
        return f"/aws/lambda-microvms/{self.name}"


class Sandbox:
    """One MicroVM's lifecycle: build, run, suspend, resume, terminate.

    `session` is available once the VM is RUNNING. Suspend and resume preserve it:
    measured 2026-08-05, a suspend/resume cycle keeps the in-memory agent token,
    the filesystem, exec records including unacked output, running processes, and
    the endpoint URL. So a warm suspended pool is viable and needs no
    re-bootstrapping.
    """

    def __init__(
        self,
        *,
        region: str = "us-east-1",
        port: int = DEFAULT_AGENT_PORT,
        microvm_client: Any = None,
        logs_client: Any = None,
        s3_client: Any = None,
        poll_interval_sec: float = DEFAULT_POLL_INTERVAL_SEC,
    ) -> None:
        self.region = region
        self.port = port
        self.poll_interval_sec = poll_interval_sec
        self._mv = microvm_client or _client(SERVICE, region)
        self._logs = logs_client
        self._s3 = s3_client
        self.image: Image | None = None
        self.microvm_id: str | None = None
        self.agent_token: str | None = None
        self.session: Session | None = None

    def __enter__(self) -> Sandbox:
        return self

    def __exit__(self, *_: object) -> None:
        self.terminate()

    # -- image -------------------------------------------------------------

    def build_image(
        self,
        *,
        name: str,
        binary: str | Path,
        bucket: str,
        build_role_arn: str,
        dockerfile: str | None = None,
        base_image: str = DEFAULT_BASE_IMAGE,
        architecture: str = "ARM_64",
        memory_mib: int = DEFAULT_MEMORY_MIB,
        hooks: dict[str, Any] | None = None,
        tags: dict[str, str] | None = None,
        build_timeout_sec: float = DEFAULT_IMAGE_BUILD_TIMEOUT_SEC,
        client_token: str | None = None,
    ) -> Image:
        """Uploads the artifact, creates the image, and waits for it to be usable."""
        s3 = self._s3 or _client("s3", self.region)
        key = f"{name}.zip"
        s3.put_object(Bucket=bucket, Key=key, Body=build_artifact(binary, dockerfile))

        created = self._mv.create_microvm_image(
            name=name,
            baseImageArn=base_image_arn(self.region, base_image),
            buildRoleArn=build_role_arn,
            codeArtifact={"uri": f"s3://{bucket}/{key}"},
            cpuConfigurations=[{"architecture": architecture}],
            resources=[{"minimumMemoryInMiB": memory_mib}],
            hooks=hooks or default_hooks(self.port),
            tags=tags or {},
            # A clientToken must never be a pure function of content or of a stable
            # resource identity. It is a *permanent* idempotency key: delete an
            # image and recreate it from the same bytes under the same name, and
            # the service replays the original create as a no-op. The result sits
            # in CREATING with its builds never scheduled, cannot be deleted
            # (CREATING forbids it) and its only version cannot be dropped (it is
            # the last one). Two images were wedged that way for ~15 hours.
            clientToken=client_token or f"create-{name}-{secrets.token_hex(4)}",
        )
        identifier = created.get("imageIdentifier") or created.get("imageArn")
        image = Image(identifier=str(identifier), name=name)
        self.image = image
        self._wait_for_image(image.identifier, deadline=time.time() + build_timeout_sec)
        return image

    def _wait_for_image(self, image_id: str, *, deadline: float) -> dict[str, Any]:
        """Waits for CREATED, distinguishing a stalled build from a slow one.

        CREATING covers both a build in progress and a build the service never
        scheduled — the clientToken-replay signature above. Probing the build list
        after a grace period turns a 45-minute silent wait into an actionable
        failure.
        """
        started = time.time()
        probed = False
        while time.time() < deadline:
            image = self._mv.get_microvm_image(imageIdentifier=image_id)
            state = image.get("state")
            if state in IMAGE_READY_STATES:
                return image
            if state and "FAILED" in state:
                raise RuntimeError(f"image build failed: {state} {image.get('stateReason')}")

            elapsed = time.time() - started
            if not probed and elapsed > STALL_GRACE_SEC:
                probed = True
                self._probe_stalled_build(image_id, elapsed)
            time.sleep(max(self.poll_interval_sec, 15.0))
        raise RuntimeError(f"image {image_id} did not become usable in time")

    def _probe_stalled_build(self, image_id: str, elapsed: float) -> None:
        try:
            versions = self._mv.list_microvm_image_versions(imageIdentifier=image_id)
            version = versions["items"][0]["imageVersion"]
            builds = self._mv.list_microvm_image_builds(
                imageIdentifier=image_id, imageVersion=version
            )
            states = [b.get("state") for b in builds.get("items", [])]
        except Exception:  # noqa: BLE001 - a best-effort probe must never break the wait
            return
        if states and all(s == "PENDING" for s in states):
            raise RuntimeError(
                f"build never scheduled after {elapsed:.0f}s: all builds still PENDING "
                f"({states}). This is the clientToken replay signature — the image is "
                "wedged and cannot be deleted."
            )

    # -- run ---------------------------------------------------------------

    def run(
        self,
        *,
        image_identifier: str | None = None,
        execution_role_arn: str,
        agent_token: str | None = None,
        max_idle_sec: int = 600,
        suspended_sec: int = 600,
        auto_resume: bool = False,
        max_duration_sec: int = 3600,
        egress: bool = False,
        ready_timeout_sec: float = 300.0,
        client_token: str | None = None,
    ) -> Session:
        """Launches a MicroVM, waits for RUNNING, and returns its `Session`.

        The agent token is delivered through `runHookPayload`, which is what keeps
        it out of the shared image snapshot. That is safe because the platform
        forwards no external traffic until the run hook returns 200 — so a per-VM
        secret delivered at launch wins the first-writer race through the endpoint.

        The daemon reads it one JSON parse deeper than you would expect: the
        platform wraps the string, so the hook body is
        `{"runHookPayload": "{\\"agent_token\\": \\"...\\"}"}`.

        `suspended_sec` is a sharp edge for anyone planning to suspend
        deliberately: the launch-time idle policy *terminates* a suspended VM after
        that window, so a "resume later" affordance silently stops working once it
        passes.
        """
        identifier = image_identifier or (self.image.identifier if self.image else None)
        if not identifier:
            raise ValueError("no image: pass image_identifier or call build_image first")

        token = agent_token or secrets.token_urlsafe(32)
        connectors = [ingress_connector_arn(self.region)]
        kwargs: dict[str, Any] = {
            "imageIdentifier": identifier,
            "executionRoleArn": execution_role_arn,
            "ingressNetworkConnectors": connectors,
            "idlePolicy": {
                # Bounds the cost of a crashed caller: an abandoned VM suspends and
                # then terminates instead of billing to the maximumDuration ceiling.
                # Idle is measured by inbound traffic through the proxy.
                "maxIdleDurationSeconds": max_idle_sec,
                "suspendedDurationSeconds": suspended_sec,
                "autoResumeEnabled": auto_resume,
            },
            "maximumDurationInSeconds": max_duration_sec,
            "runHookPayload": json.dumps({"agent_token": token}),
            "clientToken": client_token or f"run-{secrets.token_hex(8)}",
        }
        if egress:
            kwargs["egressNetworkConnectors"] = [
                ingress_connector_arn(self.region).replace("ALL_INGRESS", "INTERNET_EGRESS")
            ]

        run = self._mv.run_microvm(**kwargs)
        self.microvm_id = run["microvmId"]
        self.agent_token = token
        endpoint = self._wait_for_running(run.get("endpoint", ""), timeout=ready_timeout_sec)

        self.session = Session(
            endpoint=endpoint,
            agent_token=token,
            microvm_id=self.microvm_id,
            microvm_client=self._mv,
            port=self.port,
        )
        return self.session

    def _wait_for_running(self, endpoint: str, *, timeout: float) -> str:
        """Polls to RUNNING, failing fast on a terminal state with `stateReason`.

        Failing fast is the whole value: a VM that reaches a terminal state before
        RUNNING died during startup, and for a hook-serving daemon that almost
        always means a lifecycle hook failed. Polling through it wastes minutes and
        then reports a connection error that hides the cause — and by then the VM is
        gone, so `stateReason` is the only evidence left.
        """
        assert self.microvm_id is not None
        deadline = time.time() + timeout
        while time.time() < deadline:
            got = self._mv.get_microvm(microvmIdentifier=self.microvm_id)
            state = got.get("state")
            if state == "RUNNING":
                return got.get("endpoint") or endpoint
            if state in TERMINAL_STATES:
                raise RuntimeError(
                    f"microvm {self.microvm_id} reached {state} before RUNNING: "
                    f"{got.get('stateReason') or 'no stateReason'}"
                )
            time.sleep(self.poll_interval_sec)
        raise RuntimeError(f"microvm {self.microvm_id} never reached RUNNING within {timeout:.0f}s")

    # -- suspend / resume / terminate --------------------------------------

    def suspend(self, *, timeout: float = 300.0) -> str:
        """Freezes the VM. Returns the state reached.

        A freeze and restore, not a stop and start: the guest keeps its memory, so
        everything survives. The one thing that does not is the guest's view of
        time — it observes the whole suspension as a single jump, so any timeout,
        lease, or TLS session a running command holds expires at once on resume.
        """
        if not self.microvm_id:
            raise RuntimeError("nothing to suspend")
        self._mv.suspend_microvm(microvmIdentifier=self.microvm_id)
        return self._wait_for_state({"SUSPENDED", "TERMINATED"}, timeout=timeout)

    def resume(self, *, timeout: float = 300.0) -> Session:
        """Thaws the VM and returns a usable `Session`.

        No token re-delivery and no re-bootstrap: the in-memory token survived. The
        session's proxy token is dropped, because a token minted against the
        pre-suspend instance is not guaranteed to validate and that rejection reads
        exactly like a dead daemon.
        """
        if not self.microvm_id or self.session is None:
            raise RuntimeError("nothing to resume")
        self._mv.resume_microvm(microvmIdentifier=self.microvm_id)
        state = self._wait_for_state({"RUNNING"}, timeout=timeout)
        if state != "RUNNING":
            raise RuntimeError(f"resume left the microvm in {state}")
        got = self._mv.get_microvm(microvmIdentifier=self.microvm_id)
        self.session.rebind(got.get("endpoint") or self.session.endpoint)
        return self.session

    def _wait_for_state(self, want: set[str], *, timeout: float) -> str:
        assert self.microvm_id is not None
        deadline = time.time() + timeout
        while time.time() < deadline:
            state = self._mv.get_microvm(microvmIdentifier=self.microvm_id).get("state")
            if state in want:
                return str(state)
            time.sleep(self.poll_interval_sec)
        raise RuntimeError(f"microvm {self.microvm_id} never reached {want} within {timeout:.0f}s")

    def terminate(self, *, delete_image: bool = False, delete_log_group: bool = False) -> None:
        """Tears down, best-effort, never raising.

        Never raising because this runs in a `finally`: an exception here replaces
        the real failure with a teardown failure, and the real failure is the one
        worth reading.
        """
        if self.session is not None:
            self.session.close()
            self.session = None
        if self.microvm_id:
            with contextlib.suppress(Exception):
                self._mv.terminate_microvm(microvmIdentifier=self.microvm_id)
        if delete_log_group and self.image:
            self.delete_build_log_group()
        if delete_image and self.image:
            self.delete_image()

    def delete_build_log_group(self) -> bool:
        """Deletes the log group the service created for the build.

        Separate from `terraform destroy`, which never owned it. Six of these
        accumulated before anyone noticed — storage-only cost, but a leak is a leak.
        """
        if not self.image:
            return False
        logs = self._logs or _client("logs", self.region)
        try:
            logs.delete_log_group(logGroupName=self.image.build_log_group)
        except Exception:  # noqa: BLE001
            return False
        return True

    def delete_image(self, *, attempts: int = 20, backoff_sec: float = 15.0) -> bool:
        """Deletes the image and every version but the last, retrying.

        Retrying is not politeness: an image in CREATING refuses deletion, and a VM
        still terminating holds a reference. This retry loop is the difference
        between a clean account and a billed leak.
        """
        if not self.image:
            return False
        image_id = self.image.identifier
        for _ in range(attempts):
            try:
                versions = self._mv.list_microvm_image_versions(imageIdentifier=image_id)
                # Every version but the first: the last remaining version cannot be
                # deleted on its own, only with the image.
                for item in versions.get("items", [])[1:]:
                    self._mv.delete_microvm_image_version(
                        imageIdentifier=image_id, imageVersion=item["imageVersion"]
                    )
                self._mv.delete_microvm_image(imageIdentifier=image_id)
                return True
            except Exception:  # noqa: BLE001
                time.sleep(backoff_sec)
        return False


def _client(service: str, region: str) -> Any:
    """A boto3 client with standard retries, imported lazily.

    Lazy so `Session` and the error taxonomy stay importable without boto3 or
    credentials, which is what makes the test suite runnable with no AWS at all.
    """
    import boto3
    from botocore.config import Config as BotoConfig

    return boto3.Session(region_name=region).client(
        service, config=BotoConfig(retries={"max_attempts": 10, "mode": "standard"})
    )
