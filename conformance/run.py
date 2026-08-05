#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["boto3>=1.40", "httpx>=0.27"]
# ///
"""Live conformance run for microvms-agentd against real Lambda MicroVMs.

Builds a MicroVM image whose CMD is the daemon, runs one instance, drives every
protocol rule through the platform's own endpoint, and tears everything down
whether it passed or failed.

The model in `model/` and the turmoil tier prove properties about our own code.
This proves the parts only the real service can answer: hook path and ordering,
endpoint proxy auth, whether an omitted cwd really inherits the image WORKDIR,
and whether the daemon survives whatever the platform does to its port before
bootstrap.

Usage:
    conformance/run.py --binary target/aarch64-unknown-linux-musl/release/agentd
"""

from __future__ import annotations

import argparse
import io
import json
import os
import secrets
import subprocess
import sys
import time
import zipfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import boto3
import httpx
from botocore.config import Config as BotoConfig

SERVICE = "lambda-microvms"
REGION = os.environ.get("AWS_REGION", "us-east-1")
AGENT_PORT = 9000
HOOK_TIMEOUT_SEC = 30
# 8 GiB is the size Harbor's provider settled on as its floor; the daemon itself
# is happy in far less, and a smaller baseline is also a sharper test of the
# memory bounds.
BASELINE_MEMORY_MIB = 1024
IMAGE_BUILD_TIMEOUT_SEC = 45 * 60
STALL_GRACE_SEC = 240
# The Lambda-managed connector that lets the endpoint proxy forward inbound
# traffic to the VM. Egress is deliberately omitted: the daemon needs no outbound
# network, and leaving it off is one less thing a task workload can reach.
ALL_INGRESS_ARN = (
    f"arn:aws:lambda:{REGION}:aws:network-connector:aws-network-connector:ALL_INGRESS"
)


@dataclass
class Results:
    """Every check's outcome, so the summary reports facts rather than a feeling."""

    passed: list[str] = field(default_factory=list)
    failed: list[tuple[str, str]] = field(default_factory=list)

    def check(self, name: str, ok: bool, detail: str = "") -> bool:
        if ok:
            self.passed.append(name)
            print(f"  PASS  {name}" + (f" — {detail}" if detail else ""))
        else:
            self.failed.append((name, detail))
            print(f"  FAIL  {name} — {detail}")
        return ok

    def eq(self, name: str, actual: Any, expected: Any) -> bool:
        return self.check(name, actual == expected, f"expected {expected!r}, got {actual!r}")


def sh(cmd: list[str], cwd: Path | None = None) -> str:
    proc = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    if proc.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd)} failed:\n{proc.stdout}\n{proc.stderr}")
    return proc.stdout


def build_artifact(binary: Path) -> bytes:
    """Zips the daemon with a Dockerfile that makes it the container CMD.

    `ENTRYPOINT []` plus `CMD ["/agentd"]` is the deployment invariant the trust
    boundary rests on: it is what guarantees no task workload runs before the
    platform's run hook lands. It is also what makes an omitted `cwd` inherit the
    image WORKDIR, since the daemon's own cwd is the image's.
    """
    dockerfile = "\n".join(
        [
            "FROM public.ecr.aws/amazonlinux/amazonlinux:2023-minimal",
            "COPY agentd /agentd",
            "RUN chmod 0755 /agentd",
            # A baked WORKDIR is the only way to test cwd inheritance: every
            # public ARM64 base image we checked leaves WorkingDir empty, so
            # there would otherwise be nothing to inherit.
            "RUN mkdir -p /opt/baked-workdir",
            "WORKDIR /opt/baked-workdir",
            f"ENV AGENTD_PORT={AGENT_PORT}",
            "ENV AGENTD_LOG=info",
            f"EXPOSE {AGENT_PORT}",
            "ENTRYPOINT []",
            'CMD ["/agentd"]',
            "",
        ]
    )
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("Dockerfile", dockerfile)
        info = zipfile.ZipInfo("agentd")
        info.external_attr = 0o755 << 16
        archive.writestr(info, binary.read_bytes())
    return buffer.getvalue()


def wait_for_image(mv: Any, image_id: str, *, deadline: float) -> dict[str, Any]:
    """Waits for an image to reach CREATED, distinguishing stalled from slow.

    An image state of CREATING covers both a build in progress and a build the
    service never scheduled. The second happens when a clientToken replays a
    create that was already satisfied, and the resulting image can never be
    deleted — its state forbids deletion and its only version cannot be dropped
    because it is the last one. Probing the build list after a grace period turns
    a 45-minute silent wait into an actionable failure.
    """
    started = time.time()
    probed = False
    while time.time() < deadline:
        image = mv.get_microvm_image(imageIdentifier=image_id)
        state = image.get("state")
        if state in ("CREATED", "ACTIVE", "AVAILABLE"):
            return image
        if state and "FAILED" in state:
            raise RuntimeError(f"image build failed: {state} {image.get('stateReason')}")

        elapsed = time.time() - started
        if not probed and elapsed > STALL_GRACE_SEC:
            probed = True
            try:
                versions = mv.list_microvm_image_versions(imageIdentifier=image_id)
                version = versions["items"][0]["imageVersion"]
                builds = mv.list_microvm_image_builds(
                    imageIdentifier=image_id, imageVersion=version
                )
                states = [b.get("state") for b in builds.get("items", [])]
                if states and all(s == "PENDING" for s in states):
                    raise RuntimeError(
                        f"build never scheduled after {elapsed:.0f}s: all builds "
                        f"still PENDING ({states}). This is the clientToken replay "
                        "signature — the image is wedged and cannot be deleted."
                    )
                print(f"    build states after {elapsed:.0f}s: {states}")
            except RuntimeError:
                raise
            except Exception as exc:  # best-effort probe; never break the wait
                print(f"    (build probe failed, continuing: {exc})")

        print(f"    image {state} ({elapsed:.0f}s)")
        time.sleep(15)
    raise RuntimeError(f"image did not reach CREATED within {IMAGE_BUILD_TIMEOUT_SEC}s")


class Endpoint:
    """HTTP client for a MicroVM endpoint, minting proxy auth as needed.

    Every request needs an `X-aws-proxy-auth` JWE scoped to this MicroVM and this
    port set, valid at most 60 minutes. Minting sits inside the request path
    because a long run crosses that ceiling, and a mint failure has to be
    retryable rather than fatal.
    """

    def __init__(self, mv: Any, microvm_id: str, endpoint: str, agent_token: str) -> None:
        self.mv = mv
        self.microvm_id = microvm_id
        self.base = endpoint if endpoint.startswith("http") else f"https://{endpoint}"
        self.agent_token = agent_token
        self._proxy: str | None = None
        self._minted_at = 0.0

    def _proxy_auth(self) -> str:
        if self._proxy is None or time.time() - self._minted_at > 30 * 60:
            token = self.mv.create_microvm_auth_token(
                microvmIdentifier=self.microvm_id,
                expirationInMinutes=60,
                allowedPorts=[{"port": AGENT_PORT}],
            )
            # `authToken` is a map of header name to value, not a bare string:
            # the API is shaped for schemes that need more than one header.
            self._proxy = token["authToken"]["X-aws-proxy-auth"]
            self._minted_at = time.time()
        return self._proxy

    def request(
        self,
        method: str,
        path: str,
        *,
        token: str | None = "",
        json_body: Any = None,
        content: bytes | None = None,
        timeout: float = 60.0,
    ) -> httpx.Response:
        # The port header is not optional: the proxy needs to know which of the
        # token's allowed ports this request is for.
        headers: dict[str, Any] = {
            "X-aws-proxy-auth": self._proxy_auth(),
            "X-aws-proxy-port": str(AGENT_PORT),
        }
        bearer = self.agent_token if token == "" else token
        if bearer is not None:
            # Bytes, not str: httpx encodes str headers as ASCII and refuses
            # anything else, which would make the hostile-header check
            # unreachable. The daemon's whole point here is that it compares
            # header bytes without decoding them.
            value = bearer.encode("utf-8") if isinstance(bearer, str) else bearer
            headers["Authorization"] = b"Bearer " + value
        with httpx.Client(timeout=timeout, verify=True) as client:
            return client.request(
                method,
                f"{self.base}{path}",
                headers=headers,
                json=json_body,
                content=content,
            )


def drive_protocol(ep: Endpoint, results: Results) -> None:
    """Exercises every protocol rule the real service can validate."""

    print("\n-- bootstrap and authorization --")
    health = ep.request("GET", "/v1/health", token=None)
    results.eq("health reachable through the endpoint", health.status_code, 200)
    body = health.json() if health.status_code == 200 else {}
    # The platform delivered the token through runHookPayload before forwarding
    # any external traffic, so by the time we can reach the VM at all it must
    # already be bootstrapped. This single assertion is the hook-ordering
    # guarantee, observed rather than quoted from documentation.
    results.eq("platform ran the run hook before forwarding traffic", body.get("bootstrapped"), True)

    # The hook body is the platform's envelope, not our payload directly: the
    # string given to RunMicrovm arrives wrapped as {"runHookPayload": "<it>"}.
    def hook_body(token: str) -> dict[str, str]:
        return {"runHookPayload": json.dumps({"agent_token": token})}

    hijack = ep.request(
        "POST",
        "/aws/lambda-microvms/runtime/v1/run",
        token=None,
        json_body=hook_body("attacker-token"),
    )
    results.eq("post-bootstrap hijack refused with 409", hijack.status_code, 409)

    replay = ep.request(
        "POST",
        "/aws/lambda-microvms/runtime/v1/run",
        token=None,
        json_body=hook_body(ep.agent_token),
    )
    results.eq("identical bootstrap replay accepted", replay.status_code, 200)

    unauth = ep.request("GET", "/v1/exec/nope", token="wrong-token")
    results.eq("wrong token refused with 401", unauth.status_code, 401)

    hostile = ep.request("GET", "/v1/exec/nope", token="tökén")
    results.check(
        "non-ASCII token header answered, not a dropped connection",
        hostile.status_code == 401,
        f"status {hostile.status_code}",
    )

    print("\n-- exec --")
    started = ep.request(
        "POST", "/v1/exec/start", json_body={"exec_id": "c1", "command": ["/bin/sh", "-c", "echo live; pwd; id -u"]}
    )
    results.eq("exec start accepted", started.status_code, 200)
    outcome = await_exec(ep, "c1")
    results.eq("exec exited 0", outcome.get("exit_code"), 0)
    stdout = outcome.get("stdout", "")
    results.check("exec captured stdout", "live" in stdout, repr(stdout))
    # The daemon is the container CMD, so its cwd is the image WORKDIR and an
    # omitted cwd must land there rather than at /.
    results.check(
        "omitted cwd inherits the image WORKDIR",
        "/opt/baked-workdir" in stdout,
        repr(stdout),
    )

    retry = ep.request(
        "POST", "/v1/exec/start", json_body={"exec_id": "c1", "command": ["/bin/sh", "-c", "echo MUST_NOT_RUN"]}
    )
    results.eq("retried start accepted", retry.status_code, 200)
    after = ep.request("GET", "/v1/exec/c1").json()
    results.check(
        "retried start did not spawn a second child",
        "MUST_NOT_RUN" not in after.get("stdout", ""),
        repr(after.get("stdout")),
    )

    for name, command in [("empty", [""]), ("comment-only", ["# nothing"]), ("unbalanced brace", ["echo A } echo B"])]:
        exec_id = f"sh-{name.split()[0]}"
        ep.request("POST", "/v1/exec/start", json_body={"exec_id": exec_id, "command": command, "shell": True})
        got = await_exec(ep, exec_id)
        results.eq(f"{name} shell command exits 0", got.get("exit_code"), 0)
        if name == "unbalanced brace":
            results.check(
                "unbalanced brace did not escape into a second command",
                got.get("stdout", "").strip() == "A } echo B",
                repr(got.get("stdout")),
            )

    ack = ep.request("POST", "/v1/exec/c1/ack")
    results.eq("ack accepted", ack.status_code, 200)
    reack = ep.request("POST", "/v1/exec/c1/ack")
    results.eq("second ack refused with 409", reack.status_code, 409)

    unknown = ep.request("GET", "/v1/exec/never-existed")
    results.eq("unknown exec id is 404", unknown.status_code, 404)
    malformed = ep.request("POST", "/v1/exec/start", json_body={"bogus": True})
    results.eq("malformed body is 400, not 404", malformed.status_code, 400)

    print("\n-- large output --")
    ep.request(
        "POST",
        "/v1/exec/start",
        json_body={
            "exec_id": "noisy",
            # 32 MiB against an 8 MiB cap: the daemon must truncate and stay up,
            # not grow until the VM's OOM killer takes it.
            "command": ["/bin/sh", "-c", "dd if=/dev/zero bs=1M count=32 2>/dev/null | tr '\\0' 'x'"],
        },
    )
    noisy = await_exec(ep, "noisy", timeout=180)
    results.eq("noisy command still exits 0", noisy.get("exit_code"), 0)
    results.check("output past the cap was truncated", noisy.get("truncated") is True, str(noisy.get("truncated")))
    results.check(
        "daemon survived the truncation",
        ep.request("GET", "/v1/health", token=None).status_code == 200,
        "health after truncation",
    )

    print("\n-- file transfer --")
    wrote = ep.request(
        "PUT", "/v1/fs/file?path=/tmp/live.txt&mode=644", content=b"written through the endpoint"
    )
    results.check("single file write accepted", wrote.status_code in (200, 204), str(wrote.status_code))
    read = ep.request("GET", "/v1/fs/file?path=/tmp/live.txt")
    results.eq("single file read returns the bytes", read.content, b"written through the endpoint")
    results.eq(
        "read of an absent file is 404",
        ep.request("GET", "/v1/fs/file?path=/tmp/absent").status_code,
        404,
    )
    results.eq("missing path key is 400", ep.request("GET", "/v1/fs/file").status_code, 400)

    # A symlink must survive the round trip: harnesses pack them deliberately,
    # and a daemon that refuses links breaks real uploads.
    ep.request(
        "POST",
        "/v1/exec/start",
        json_body={
            "exec_id": "mktree",
            "command": ["mkdir -p /tmp/tree/sub && echo payload > /tmp/tree/a.txt && ln -sf a.txt /tmp/tree/link && echo deep > /tmp/tree/sub/b.txt"],
            "shell": True,
        },
    )
    results.eq("tree created for the round trip", await_exec(ep, "mktree").get("exit_code"), 0)

    archive = ep.request("GET", "/v1/fs/tar?path=/tmp/tree")
    results.eq("tar download succeeded", archive.status_code, 200)
    uploaded = ep.request("PUT", "/v1/fs/tar?path=/tmp/dest", content=archive.content)
    results.check("tar upload accepted", uploaded.status_code in (200, 204), str(uploaded.status_code))

    ep.request(
        "POST",
        "/v1/exec/start",
        json_body={
            "exec_id": "verify",
            "command": ["readlink /tmp/dest/link; cat /tmp/dest/link; cat /tmp/dest/sub/b.txt"],
            "shell": True,
        },
    )
    verify = await_exec(ep, "verify")
    results.check(
        "symlink survived the round trip as a symlink",
        verify.get("stdout", "").startswith("a.txt"),
        repr(verify.get("stdout")),
    )
    results.check(
        "symlink still resolves to its target's content",
        "payload" in verify.get("stdout", ""),
        repr(verify.get("stdout")),
    )

    print("\n-- hostile archives --")
    for name, archive_bytes, expect in build_hostile_archives():
        response = ep.request("PUT", "/v1/fs/tar?path=/tmp/hostile", content=archive_bytes)
        results.check(
            f"hostile archive refused: {name}",
            response.status_code == expect,
            f"expected {expect}, got {response.status_code}: {response.text[:120]}",
        )

    ep.request(
        "POST",
        "/v1/exec/start",
        json_body={"exec_id": "escaped", "command": ["ls /escaped.txt /tmp/escaped.txt 2>&1 | head -3; echo done"], "shell": True},
    )
    escaped = await_exec(ep, "escaped")
    results.check(
        "nothing escaped the extraction root",
        "No such file" in escaped.get("stdout", "") or "cannot access" in escaped.get("stdout", ""),
        repr(escaped.get("stdout")),
    )


def build_hostile_archives() -> list[tuple[str, bytes, int]]:
    """Archives hand-built with tarfile, since GNU tar sanitizes several of these."""
    import tarfile

    def make(members: list[tuple[str, str, str | None, bytes]]) -> bytes:
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode="w") as tar:
            for name, kind, target, data in members:
                info = tarfile.TarInfo(name)
                if kind == "file":
                    info.size = len(data)
                    tar.addfile(info, io.BytesIO(data))
                elif kind == "sym":
                    info.type = tarfile.SYMTYPE
                    info.linkname = target or ""
                    tar.addfile(info)
                elif kind == "dev":
                    info.type = tarfile.CHRTYPE
                    info.devmajor, info.devminor = 1, 3
                    tar.addfile(info)
        return buffer.getvalue()

    return [
        ("parent traversal", make([("../../escaped.txt", "file", None, b"pwned")]), 400),
        ("absolute link target", make([("link", "sym", "/etc/passwd", b"")]), 400),
        (
            "symlink redirect",
            make([("s", "sym", "..", b""), ("s/escaped.txt", "file", None, b"pwned")]),
            400,
        ),
        ("character device", make([("dev", "dev", None, b"")]), 400),
    ]


def await_exec(ep: Endpoint, exec_id: str, timeout: float = 60.0) -> dict[str, Any]:
    deadline = time.time() + timeout
    last: dict[str, Any] = {}
    while time.time() < deadline:
        response = ep.request("GET", f"/v1/exec/{exec_id}")
        if response.status_code != 200:
            return {"error": response.status_code, "body": response.text[:200]}
        last = response.json()
        if last.get("phase") in ("exited", "acked"):
            return last
        time.sleep(2)
    return last | {"error": "timeout"}


def read_daemon_logs(logs: Any, image_name: str, microvm_id: str) -> list[str]:
    """Pulls the daemon's own log lines, which carry the loopback measurement."""
    lines: list[str] = []
    for group in (f"/aws/lambda-microvms/{image_name}", "/aws/lambda-microvms"):
        try:
            streams = logs.describe_log_streams(
                logGroupName=group, orderBy="LastEventTime", descending=True, limit=5
            )
            for stream in streams.get("logStreams", []):
                events = logs.get_log_events(
                    logGroupName=group,
                    logStreamName=stream["logStreamName"],
                    limit=200,
                    startFromHead=False,
                )
                lines.extend(e["message"] for e in events.get("events", []))
            if lines:
                print(f"    log group {group}: {len(lines)} lines")
                return lines
        except Exception as exc:
            print(f"    log group {group} unavailable: {type(exc).__name__}")
    return lines


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--keep", action="store_true", help="skip teardown (leaks resources)")
    args = parser.parse_args()

    repo = Path(__file__).resolve().parent.parent
    infra = repo / "conformance" / "infra"
    binary = (repo / args.binary).resolve() if not args.binary.is_absolute() else args.binary
    if not binary.exists():
        print(f"binary not found: {binary}")
        return 2

    outputs = json.loads(sh(["terraform", "output", "-json"], cwd=infra))
    bucket = outputs["s3_bucket"]["value"]
    build_role = outputs["build_role_arn"]["value"]
    execution_role = outputs["execution_role_arn"]["value"]
    print(f"infra: bucket={bucket}")

    session = boto3.Session(region_name=REGION)
    mv = session.client(SERVICE, config=BotoConfig(retries={"max_attempts": 10, "mode": "standard"}))
    s3 = session.client("s3")
    logs = session.client("logs")

    run_id = secrets.token_hex(4)
    image_name = f"agentd-conformance-{run_id}"
    agent_token = secrets.token_urlsafe(32)
    results = Results()
    image_id: str | None = None
    microvm_id: str | None = None

    try:
        print("\n== artifact ==")
        artifact = build_artifact(binary)
        key = f"{image_name}.zip"
        s3.put_object(Bucket=bucket, Key=key, Body=artifact)
        artifact_uri = f"s3://{bucket}/{key}"
        print(f"  uploaded {len(artifact)} bytes to {artifact_uri}")

        print("\n== image ==")
        created = mv.create_microvm_image(
            name=image_name,
            baseImageArn="arn:aws:lambda:us-east-1:aws:microvm-image:al2023-1",
            buildRoleArn=build_role,
            codeArtifact={"uri": artifact_uri},
            cpuConfigurations=[{"architecture": "ARM_64"}],
            resources=[{"minimumMemoryInMiB": BASELINE_MEMORY_MIB}],
            hooks={
                "port": AGENT_PORT,
                "microvmImageHooks": {
                    "ready": "ENABLED",
                    "readyTimeoutInSeconds": HOOK_TIMEOUT_SEC,
                    "validate": "ENABLED",
                    "validateTimeoutInSeconds": HOOK_TIMEOUT_SEC,
                },
                "microvmHooks": {
                    "run": "ENABLED",
                    "runTimeoutInSeconds": HOOK_TIMEOUT_SEC,
                    "terminate": "ENABLED",
                    "terminateTimeoutInSeconds": HOOK_TIMEOUT_SEC,
                },
            },
            tags={"agentd:purpose": "conformance", "agentd:run": run_id},
            # Scoped to this run, never a pure function of the artifact's
            # content. A content-derived token is a permanent idempotency key:
            # delete the image and rebuild the same bytes, and the service
            # replays the original create as a no-op, wedging an image that then
            # cannot be deleted at all.
            clientToken=f"create-{image_name}-{run_id}",
        )
        image_id = created.get("imageIdentifier") or created.get("imageArn")
        print(f"  image {image_id} state={created.get('state')}")
        wait_for_image(mv, image_id, deadline=time.time() + IMAGE_BUILD_TIMEOUT_SEC)
        print("  image CREATED")

        print("\n== run ==")
        run = mv.run_microvm(
            imageIdentifier=image_id,
            executionRoleArn=execution_role,
            # Connectors are ARNs, not bare names: the literal "ALL_INGRESS" is
            # rejected with "Malformed network connector ARN".
            ingressNetworkConnectors=[ALL_INGRESS_ARN],
            idlePolicy={
                # Bounds the cost of a crash in this script: an abandoned VM
                # suspends and then terminates instead of billing to the ceiling.
                "maxIdleDurationSeconds": 600,
                "suspendedDurationSeconds": 600,
                "autoResumeEnabled": False,
            },
            maximumDurationInSeconds=3600,
            runHookPayload=json.dumps({"agent_token": agent_token}),
            clientToken=f"run-{image_name}-{run_id}",
        )
        microvm_id = run["microvmId"]
        endpoint = run.get("endpoint", "")
        print(f"  microvm {microvm_id} state={run.get('state')} endpoint={endpoint}")

        # States are PENDING -> RUNNING -> SUSPENDING/TERMINATING -> TERMINATED.
        # Anything terminal before RUNNING means the VM died during startup,
        # which for this daemon almost always means a lifecycle hook failed.
        # Polling through it would waste minutes and then report a connection
        # error, hiding the actual cause.
        for _ in range(60):
            got = mv.get_microvm(microvmIdentifier=microvm_id)
            state = got.get("state")
            if state == "RUNNING":
                endpoint = got.get("endpoint", endpoint)
                print(f"  microvm RUNNING endpoint={endpoint}")
                break
            if state in ("TERMINATED", "TERMINATING", "SUSPENDED", "SUSPENDING"):
                raise RuntimeError(
                    f"microvm reached {state} before RUNNING: "
                    f"{got.get('stateReason') or 'no stateReason'}"
                )
            print(f"    microvm {state}")
            time.sleep(5)
        else:
            raise RuntimeError("microvm never reached RUNNING")

        ep = Endpoint(mv, microvm_id, endpoint, agent_token)
        print("\n== protocol ==")
        drive_protocol(ep, results)

        print("\n== daemon logs ==")
        lines = read_daemon_logs(logs, image_name, microvm_id)
        hook_lines = [line for line in lines if "hook" in line.lower()]
        for line in hook_lines[:10]:
            print(f"    {line.strip()[:200]}")
        results.check(
            "daemon logs reached CloudWatch under /aws/lambda-microvms/",
            bool(lines),
            f"{len(lines)} lines",
        )

    finally:
        if args.keep:
            print("\n== teardown SKIPPED (--keep) ==")
        else:
            print("\n== teardown ==")
            if microvm_id:
                try:
                    mv.terminate_microvm(microvmIdentifier=microvm_id)
                    print(f"  terminated {microvm_id}")
                except Exception as exc:
                    print(f"  terminate failed: {exc}")
            # The service creates the build log group itself, so Terraform never
            # owns it and `destroy` leaves it behind. Six of them accumulated
            # across the runs that built this script before anyone noticed —
            # storage-only cost, but a leak is a leak, and "the stack destroyed
            # cleanly" was the wrong conclusion to draw from Terraform's output.
            try:
                group = f"/aws/lambda-microvms/{image_name}"
                logs.delete_log_group(logGroupName=group)
                print(f"  deleted log group {group}")
            except Exception as exc:
                print(f"  log group delete skipped: {type(exc).__name__}")

            if image_id:
                for _ in range(20):
                    try:
                        versions = mv.list_microvm_image_versions(imageIdentifier=image_id)
                        for item in versions.get("items", [])[1:]:
                            mv.delete_microvm_image_version(
                                imageIdentifier=image_id, imageVersion=item["imageVersion"]
                            )
                        mv.delete_microvm_image(imageIdentifier=image_id)
                        print(f"  deleted image {image_id}")
                        break
                    except Exception as exc:
                        # An image in CREATING refuses deletion, and a VM still
                        # terminating holds a reference. Retrying is the whole
                        # difference between a clean account and a billed leak.
                        print(f"  image delete retry: {type(exc).__name__}")
                        time.sleep(15)

    print("\n== summary ==")
    print(f"  passed: {len(results.passed)}")
    print(f"  failed: {len(results.failed)}")
    for name, detail in results.failed:
        print(f"    FAIL {name}: {detail}")
    return 0 if not results.failed else 1


if __name__ == "__main__":
    sys.exit(main())
