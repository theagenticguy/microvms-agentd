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
import re
import secrets
import time
import zipfile
from collections.abc import Callable
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any

from .session import Session
from .sizing import DEFAULT_BASELINE_MIB, SizeClass, size_class_for
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

#: The subset of `TERMINAL_STATES` from which nothing comes back. Separate from
#: `TERMINAL_STATES` because SUSPENDED is a death *before* RUNNING and an ordinary
#: waypoint on the resume path — a resume that failed fast on SUSPENDED would fail
#: on every resume, since that is the state the VM is in when the call is made.
DEAD_STATES = frozenset({"TERMINATED", "TERMINATING"})

DEFAULT_BASE_IMAGE = "al2023-1"
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


#: Bits of randomness folded into every idempotency token. Eight bytes, so two
#: attempts one second apart do not collide even under a retry storm.
_TOKEN_NONCE_BYTES = 8


def _create_token(scope: str) -> str:
    """An image-create idempotency token, unique per attempt by construction.

    A `clientToken` is a *permanent* idempotency key. Measured 2026-08-02, the
    expensive way: delete an image and recreate it from the same bytes under the same
    name with a token derived from that identity, and the service replays the original
    create as a no-op. The image sits in `CREATING` with its builds never scheduled,
    cannot be deleted (`CREATING` forbids it), and its only version cannot be dropped
    (it is the last one). Two images were wedged that way for ~15 hours.

    So there is no caller-supplied token parameter anywhere in this client, only a
    `token_scope` label that lands *next to* the nonce. That is the difference between
    a default a caller can override and a mistake a caller cannot write: the previous
    shape defaulted correctly and accepted `client_token=<content digest>`, which is
    precisely the value that wedges an image.
    """
    return f"create-{scope}-{secrets.token_hex(_TOKEN_NONCE_BYTES)}"


def _run_token(scope: str) -> str:
    """A run idempotency token, unique per attempt. Same rule as `_create_token`.

    Cheaper to get wrong than the image case — a replayed run returns the original
    MicroVM rather than wedging anything — but the failure is worse to read: a caller
    who asked for a second VM gets the first one's id back and two callers then drive
    the same guest.
    """
    return f"run-{scope}-{secrets.token_hex(_TOKEN_NONCE_BYTES)}"


class NetworkConnector(Enum):
    """The Lambda-managed connectors this client will name, and no others.

    A closed set rather than a string parameter, for two reasons. The API takes a
    fully-qualified ARN and rejects the bare name with "Malformed network connector
    ARN" (measured 2026-08-05), so a free-form parameter invites the one value that
    reads most natural and fails. And `SHELL_INGRESS` is deliberately absent: it
    exists in the API, but it gates `CreateMicrovmShellAuthToken`, whose documented
    flow is `ctr task exec` through a console terminal, scoped to debugging and
    recommended disabled in production. It is not a programmatic exec path despite
    the name, and this client's whole reason to exist is that no such path exists.
    Leaving it out of the enum is what makes requesting it unwriteable rather than
    merely discouraged.
    """

    ALL_INGRESS = "ALL_INGRESS"
    INTERNET_EGRESS = "INTERNET_EGRESS"


def connector_arn(region: str, connector: NetworkConnector = NetworkConnector.ALL_INGRESS) -> str:
    """The fully-qualified ARN for a managed connector in `region`.

    One interpolation for both directions. The previous shape derived the egress ARN
    by string-replacing `ALL_INGRESS` inside the ingress one, which produced a valid
    ARN only as long as the two names never became substrings of each other.
    """
    return f"arn:aws:lambda:{region}:aws:network-connector:aws-network-connector:{connector.value}"


def ingress_connector_arn(region: str) -> str:
    """The connector that lets the endpoint proxy reach the VM.

    Omitting egress is how you get a VM with no outbound network — which is the right
    default for a daemon that needs none.
    """
    return connector_arn(region, NetworkConnector.ALL_INGRESS)


@dataclass(frozen=True)
class BaseImage:
    """A base image, its Dockerfile `FROM`, and whether it declares a `WORKDIR`.

    All three in one object because the first two must agree and used to be able to
    disagree: `DEFAULT_BASE_IMAGE` named the managed base that goes in
    `baseImageArn`, while `default_dockerfile` hardcoded an unrelated registry
    literal in its `FROM`, so changing either left the other pointing somewhere
    else. Pairing them means a caller selects one thing and both fields follow.

    `working_dir` is empty for every public ARM64 base we measured
    (`al2023-minimal`, `python:3.12-slim`, `node:20-slim`, measured 2026-08-05), so
    an omitted `cwd` inherits nothing. It is a field rather than a lookup because a
    caller with a purpose-built image is the only one who can say what their image
    declares, and the client has no way to read it without pulling the manifest.
    """

    #: Goes into `baseImageArn` — the platform's managed base, not a registry ref.
    name: str
    #: Goes into the Dockerfile `FROM` — the registry ref measured alongside `name`.
    docker_ref: str
    #: What `docker inspect` reports for `WorkingDir`. Empty means it declares none.
    working_dir: str = ""


#: The base images this repo has actually built and run. `al2023-1` is the managed
#: base every measurement in `docs/PLATFORM.md` from 2026-08-06 onward used, paired
#: with the `amazonlinux:2023-minimal` registry ref the same builds used as `FROM`.
BASE_IMAGES: dict[str, BaseImage] = {
    "al2023-1": BaseImage(
        name="al2023-1",
        docker_ref="public.ecr.aws/amazonlinux/amazonlinux:2023-minimal",
        working_dir="",
    ),
}


def resolve_base_image(base_image: str | BaseImage) -> BaseImage:
    """Accepts a registered name or a caller's own `BaseImage`.

    A bare name must be one we have built against, because the name alone does not
    say what `FROM` pairs with it or what `WORKDIR` it declares — and guessing either
    is how the two fell out of step before. A caller on an unregistered base passes a
    `BaseImage` and states both.
    """
    if isinstance(base_image, BaseImage):
        return base_image
    try:
        return BASE_IMAGES[base_image]
    except KeyError:
        known = ", ".join(sorted(BASE_IMAGES))
        raise ValueError(
            f"unknown base image {base_image!r}. Pass one of: {known} — or a BaseImage "
            "naming its own docker_ref and working_dir, since the client cannot read "
            "either from the name."
        ) from None


def base_image_arn(region: str, name: str | BaseImage = DEFAULT_BASE_IMAGE) -> str:
    return f"arn:aws:lambda:{region}:aws:microvm-image:{resolve_base_image(name).name}"


#: Matches the first `FROM` of a Dockerfile, so a caller-supplied one can be checked
#: against the base image the create call names. Deliberately loose on whitespace and
#: case, and it ignores `--platform=`/`AS name` decoration, because the check exists
#: to catch a base that disagrees rather than to validate Dockerfile syntax.
_FROM_RE = re.compile(r"^\s*FROM\s+(?:--\S+\s+)*(\S+)", re.IGNORECASE | re.MULTILINE)


def dockerfile_from_ref(dockerfile: str) -> str | None:
    """The image ref in the Dockerfile's first `FROM`, or None if it has none."""
    match = _FROM_RE.search(dockerfile)
    return match.group(1) if match else None


def default_dockerfile(
    *,
    port: int = DEFAULT_AGENT_PORT,
    workdir: str | None = None,
    base_image: str | BaseImage = DEFAULT_BASE_IMAGE,
) -> str:
    """A Dockerfile that makes the daemon the container CMD.

    `ENTRYPOINT []` plus `CMD ["/agentd"]` is the deployment invariant the trust
    boundary rests on: it is what guarantees no task workload runs before the
    platform's run hook lands. It is also what makes an omitted `cwd` inherit the
    image WORKDIR, since the daemon's own cwd is the image's.

    The `FROM` is derived from `base_image` rather than written here, so it cannot
    disagree with the `baseImageArn` the create call sends.

    Note the invariant is *unenforced* — a base image that starts its own
    background process before bootstrap breaks it, and enforcing that belongs to
    whoever builds the image. See `docs/PROTOCOL.md`, "Trust boundary".
    """
    lines = [
        f"FROM {resolve_base_image(base_image).docker_ref}",
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


#: Measured 2026-08-05. Not `/aws/lambda/microvms/*` — an IAM policy granting that
#: prefix instead produces server-side builds with no logs at all, and then every
#: build failure reads `reason=unknown`, which looks like the service failing to
#: populate `stateReason` when it is the caller's own policy discarding the evidence.
BUILD_LOG_GROUP_PREFIX = "/aws/lambda-microvms"


@dataclass
class Image:
    """A built MicroVM image, and the log group the service created alongside it."""

    identifier: str
    name: str
    #: The class the requested baseline selected. Carried on the image because
    #: billing follows the baseline that was requested at *create* time, and by the
    #: time anyone asks what a run cost the request is gone.
    size: SizeClass = field(default_factory=lambda: size_class_for(DEFAULT_BASELINE_MIB))

    @property
    def build_log_group(self) -> str:
        """`/aws/lambda-microvms/<image-name>`, not `/aws/lambda/microvms/*`.

        The service creates this itself, so a Terraform stack never owns it and
        `terraform destroy` leaves it behind — "the stack destroyed cleanly" is not
        "the account is clean". Delete it in teardown.
        """
        return f"{BUILD_LOG_GROUP_PREFIX}/{self.name}"


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
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self.region = region
        self.port = port
        self.poll_interval_sec = poll_interval_sec
        # Monotonic, and injectable. Monotonic because the suspended window is a
        # duration and a wall clock that steps backward would reopen a closed one.
        # Injectable because the only other way to test a 600-second window is to
        # wait 600 seconds.
        self._clock = clock
        self._mv = microvm_client or _client(SERVICE, region)
        self._logs = logs_client
        self._s3 = s3_client
        self.image: Image | None = None
        self.microvm_id: str | None = None
        self.agent_token: str | None = None
        self.session: Session | None = None
        # The launch-time idlePolicy window, recorded because `resume` needs it and
        # `GetMicrovm` does not return it: the value only exists in the RunMicrovm
        # request, so a resume path that wants to name the window has to remember it.
        self._suspended_window_sec: int | None = None
        self._suspended_at: float | None = None

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
        base_image: str | BaseImage = DEFAULT_BASE_IMAGE,
        architecture: str = "ARM_64",
        memory_mib: int = DEFAULT_BASELINE_MIB,
        hooks: dict[str, Any] | None = None,
        tags: dict[str, str] | None = None,
        repair_guest_identity: bool = False,
        inherit_workdir: bool = False,
        build_timeout_sec: float = DEFAULT_IMAGE_BUILD_TIMEOUT_SEC,
        token_scope: str | None = None,
    ) -> Image:
        """Uploads the artifact, creates the image, and waits for it to be usable.

        `memory_mib` selects a documented size class, it does not size the VM: the
        guest will report roughly four times what is asked for, and the billed rate
        follows the request. `sizing.size_class_for` names both numbers.

        `token_scope` is a label folded into the create token for readability in
        CloudTrail. It is not the token — see `_create_token` for why the client will
        not accept one.
        """
        base = resolve_base_image(base_image)
        size = size_class_for(memory_mib)
        if inherit_workdir:
            self._require_workdir(base, dockerfile)
        if dockerfile is not None:
            self._require_matching_from(base, dockerfile)

        s3 = self._s3 or _client("s3", self.region)
        key = f"{name}.zip"
        s3.put_object(
            Bucket=bucket,
            Key=key,
            Body=build_artifact(binary, dockerfile or default_dockerfile(port=self.port)),
        )

        created = self._mv.create_microvm_image(
            name=name,
            baseImageArn=base_image_arn(self.region, base),
            buildRoleArn=build_role_arn,
            codeArtifact={"uri": f"s3://{bucket}/{key}"},
            cpuConfigurations=[{"architecture": architecture}],
            resources=[{"minimumMemoryInMiB": size.baseline_mib}],
            hooks=hooks or default_hooks(self.port),
            tags=tags or {},
            # Measured 2026-08-06, us-east-1: without this, a guest running as root
            # still gets EPERM from `sethostname` and from a bind mount over
            # `/proc/sys/kernel/random/boot_id`, because the MicroVM drops
            # CAP_SYS_ADMIN by default. Writing `/etc/machine-id` needs no
            # capability and succeeds either way, which is what makes the gap easy
            # to miss: identity repair looks like it works until you check the two
            # steps that need the kernel's permission rather than the filesystem's.
            #
            # "ALL" is the only accepted value in the 2025-09-09 API — there is no
            # way to request CAP_SYS_ADMIN alone — which is why the parameter above
            # is a boolean naming the *intent*. A list would let a caller write
            # ["CAP_SYS_ADMIN"], the request AWS rejects after the artifact upload.
            **({"additionalOsCapabilities": ["ALL"]} if repair_guest_identity else {}),
            clientToken=_create_token(token_scope or name),
        )
        identifier = created.get("imageIdentifier") or created.get("imageArn")
        image = Image(identifier=str(identifier), name=name, size=size)
        self.image = image
        self._wait_for_image(image, deadline=time.time() + build_timeout_sec)
        return image

    @staticmethod
    def _require_workdir(base: BaseImage, dockerfile: str | None) -> None:
        """Rejects working-directory inheritance when nothing declares one.

        Measured 2026-08-05: `al2023-minimal`, `python:3.12-slim`, and `node:20-slim`
        all leave `WorkingDir` empty, so "inherit the image WORKDIR" inherits `/` and
        every relative path in the caller's commands resolves somewhere they did not
        mean. Rejected rather than warned because the symptom appears in the *guest*,
        one build cycle later, as commands that run in the wrong directory rather than
        as anything about WORKDIR.
        """
        if base.working_dir:
            return
        if dockerfile is not None and re.search(r"^\s*WORKDIR\s+\S", dockerfile, re.MULTILINE):
            return
        raise ValueError(
            f"inherit_workdir=True but base image {base.name!r} declares no WorkingDir and "
            "the Dockerfile sets none. Most public ARM64 base images leave it empty "
            "(docs/PLATFORM.md, 'Most public ARM64 base images have no WORKDIR'), so there "
            "is nothing to inherit — pass `default_dockerfile(workdir=...)` or set WORKDIR "
            "in your own Dockerfile."
        )

    @staticmethod
    def _require_matching_from(base: BaseImage, dockerfile: str) -> None:
        """Rejects a Dockerfile whose `FROM` is not the selected base image.

        The build runs the Dockerfile *on top of* the base named in `baseImageArn`, so
        the two disagreeing produces an image built from something other than the
        platform base whose behavior every measurement in `docs/PLATFORM.md` describes
        — and nothing in the result says so.
        """
        found = dockerfile_from_ref(dockerfile)
        if found is None or found == base.docker_ref:
            return
        raise ValueError(
            f"the Dockerfile's FROM is {found!r} but base_image {base.name!r} pairs with "
            f"{base.docker_ref!r}. These must agree: `baseImageArn` and the FROM select the "
            "same base, and a mismatch builds against a base none of the measured platform "
            "behavior applies to. Use `default_dockerfile(base_image=...)`, or pass a "
            "BaseImage whose docker_ref matches."
        )

    def _wait_for_image(self, image: Image, *, deadline: float) -> dict[str, Any]:
        """Waits for CREATED, distinguishing a stalled build from a slow one.

        CREATING covers both a build in progress and a build the service never
        scheduled — the clientToken-replay signature above. Probing the build list
        after a grace period turns a 45-minute silent wait into an actionable
        failure.
        """
        image_id = image.identifier
        started = time.time()
        probed = False
        while time.time() < deadline:
            got = self._mv.get_microvm_image(imageIdentifier=image_id)
            state = got.get("state")
            if state in IMAGE_READY_STATES:
                return got
            if state and "FAILED" in state:
                raise RuntimeError(
                    self._build_failure_message(image, state, got.get("stateReason"))
                )

            elapsed = time.time() - started
            if not probed and elapsed > STALL_GRACE_SEC:
                probed = True
                self._probe_stalled_build(image_id, elapsed)
            time.sleep(max(self.poll_interval_sec, 15.0))
        raise RuntimeError(f"image {image_id} did not become usable in time")

    def _build_failure_message(self, image: Image, state: str, reason: Any) -> str:
        """Explains a build failure, and names the log-group prefix when there are no logs.

        Measured 2026-08-05: a build role granted `/aws/lambda/microvms/*` — the
        plausible spelling, and the wrong one — produces server-side builds that write
        no logs at all. Every failure then reads `reason=unknown`, which looks like the
        service failing to populate `stateReason` and sends the reader to investigate
        AWS rather than their own policy. Forwarding `stateReason` verbatim is what
        makes that indistinguishable, so an empty log group gets named as the more
        likely cause than a silent service.
        """
        base = f"image build failed: {state} {reason}"
        if self._build_log_group_is_empty(image):
            return (
                f"{base} — and the build log group {image.build_log_group!r} contains no "
                f"events, so the reason above is all the evidence there is. The build role "
                f"must grant logs on the {BUILD_LOG_GROUP_PREFIX}/* prefix; a policy "
                f"granting /aws/lambda/microvms/* instead produces builds with no logs and "
                f"failures that read as unknown (docs/PLATFORM.md, 'Build logs go to "
                f"{BUILD_LOG_GROUP_PREFIX}/<image-name>')."
            )
        return base

    def _build_log_group_is_empty(self, image: Image) -> bool:
        """True only when CloudWatch answers and answers empty.

        A logs client that throws, or that is absent because the caller passed none,
        must read as "unknown" rather than "empty": claiming a misconfigured IAM prefix
        on the strength of a throttled API call would send the reader after the wrong
        cause, which is the failure this whole path exists to prevent.
        """
        logs = self._logs
        if logs is None:
            with contextlib.suppress(Exception):
                logs = _client("logs", self.region)
        if logs is None:
            return False
        try:
            streams = logs.describe_log_streams(logGroupName=image.build_log_group)
        except Exception:  # noqa: BLE001 - unknown is not empty; see the docstring
            return False
        return not any(s.get("lastEventTimestamp") for s in streams.get("logStreams", []))

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
        token_scope: str | None = None,
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
        passes. `resume` reads the value recorded here to say so.
        """
        identifier = image_identifier or (self.image.identifier if self.image else None)
        if not identifier:
            raise ValueError("no image: pass image_identifier or call build_image first")

        token = agent_token or secrets.token_urlsafe(32)
        kwargs: dict[str, Any] = {
            "imageIdentifier": identifier,
            "executionRoleArn": execution_role_arn,
            "ingressNetworkConnectors": [connector_arn(self.region, NetworkConnector.ALL_INGRESS)],
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
            "clientToken": _run_token(token_scope or identifier),
        }
        if egress:
            kwargs["egressNetworkConnectors"] = [
                connector_arn(self.region, NetworkConnector.INTERNET_EGRESS)
            ]

        run = self._mv.run_microvm(**kwargs)
        self.microvm_id = run["microvmId"]
        self.agent_token = token
        self._suspended_window_sec = suspended_sec
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
        # Stamped before the wait, not after: the idlePolicy's window starts when the
        # platform begins suspending, so timing it from SUSPENDED would under-count the
        # transition and call a closed window open.
        self._suspended_at = self._clock()
        return self._wait_for_state({"SUSPENDED", "TERMINATED"}, timeout=timeout)

    def resume(self, *, timeout: float = 300.0) -> Session:
        """Thaws the VM and returns a usable `Session`.

        No token re-delivery and no re-bootstrap: the in-memory token survived. The
        session's proxy token is dropped, because a token minted against the
        pre-suspend instance is not guaranteed to validate and that rejection reads
        exactly like a dead daemon.

        Two ways this refuses rather than hangs, both because the launch-time
        `idlePolicy` *terminates* a suspended VM after `suspendedDurationSeconds` —
        so "resume later" silently stops working once that window passes, and the
        VM is gone rather than slow.
        """
        if not self.microvm_id or self.session is None:
            raise RuntimeError("nothing to resume")
        self._require_open_suspended_window()
        self._mv.resume_microvm(microvmIdentifier=self.microvm_id)
        state = self._wait_for_state({"RUNNING"}, timeout=timeout, fail_on=DEAD_STATES)
        if state != "RUNNING":
            raise RuntimeError(f"resume left the microvm in {state}")
        got = self._mv.get_microvm(microvmIdentifier=self.microvm_id)
        self.session.rebind(got.get("endpoint") or self.session.endpoint)
        # Cleared on success so the next cycle's window is measured from the next
        # suspend. Leaving it set would accumulate every suspension's elapsed time
        # into one total and reject a resume whose own window is wide open.
        self._suspended_at = None
        return self.session

    def _require_open_suspended_window(self) -> None:
        """Rejects a resume the launch-time `idlePolicy` has already made impossible.

        Checked locally, before `ResumeMicrovm`, because the answer is already known:
        the window came from *our own* `RunMicrovm` request and `GetMicrovm` does not
        return it, so the client is the only party that can name the number. Calling
        first and reading the failure would report whatever the service says about a
        terminated id, which is not the same as "the window you set at launch closed".
        """
        window = self._suspended_window_sec
        if window is None or self._suspended_at is None:
            return
        elapsed = self._clock() - self._suspended_at
        if elapsed <= window:
            return
        raise RuntimeError(
            f"microvm {self.microvm_id} has been suspended {elapsed:.0f}s, past the "
            f"{window}s suspendedDurationSeconds window set at launch — the idlePolicy "
            f"terminates a suspended VM once that window passes, so there is nothing left "
            f"to resume (docs/PLATFORM.md, '`idlePolicy`')."
        )

    def _wait_for_state(
        self, want: set[str], *, timeout: float, fail_on: frozenset[str] = frozenset()
    ) -> str:
        """Polls until a wanted state, failing fast on any `fail_on` state.

        `fail_on` exists because the resume path had the exact defect
        `_wait_for_running` was written to prevent. A VM the `idlePolicy` terminated
        during suspension never reaches RUNNING, so waiting only for RUNNING burned
        the full 300s and then reported "never reached RUNNING" — a timeout message
        that hides a cause the service had already stated in `stateReason`.

        Empty by default rather than always `DEAD_STATES`, because `suspend` *wants*
        TERMINATED: a VM that dies while suspending is a state to report, not an
        exception to raise out of the middle of a teardown.
        """
        assert self.microvm_id is not None
        deadline = time.time() + timeout
        while time.time() < deadline:
            got = self._mv.get_microvm(microvmIdentifier=self.microvm_id)
            state = got.get("state")
            if state in want:
                return str(state)
            if state in fail_on:
                raise RuntimeError(
                    f"microvm {self.microvm_id} is {state}, so it will never reach "
                    f"{sorted(want)}: {got.get('stateReason') or 'no stateReason'}. A "
                    f"suspended VM is terminated once the launch-time "
                    f"suspendedDurationSeconds window ({self._suspended_window_sec}s) "
                    f"passes (docs/PLATFORM.md, '`idlePolicy`')."
                )
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
