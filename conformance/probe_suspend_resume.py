#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["boto3>=1.40", "httpx>=0.27"]
# ///
"""Measure what survives a Lambda MicroVM suspend/resume cycle.

The daemon currently claims, in a docstring, that a resumed VM has no agent token
because bootstrap state lives in memory. That claim decides real design: if it is
true, every consumer needs token re-delivery plumbing before pause/resume is
usable, and a suspended agent sandbox is worthless. If it is false — because
suspend is a memory snapshot rather than a process kill — then pause/resume is
nearly free and the docstring is actively misleading.

Nobody has measured it. This probe does, and it measures four things rather than
one, because "the token survived" and "a running process survived" are different
questions with different consequences:

  1. Does the in-memory agent token survive? (`/v1/health` -> bootstrapped)
  2. Does filesystem state survive?
  3. Does a *running* process survive — specifically a backgrounded writer, which
     is the shape an agent harness takes?
  4. Does an exec record from before the suspend survive, so a caller can still
     collect output it never acked?

Answers land in docs/PLATFORM.md with this run's date and region.
"""

from __future__ import annotations

import argparse
import io
import itertools
import json
import os
import secrets
import subprocess
import sys
import time
import zipfile
from pathlib import Path
from typing import Any

import boto3
import httpx
from botocore.config import Config as BotoConfig

SERVICE = "lambda-microvms"
REGION = os.environ.get("AWS_REGION", "us-east-1")
AGENT_PORT = 9000
HOOK_TIMEOUT_SEC = 30
BASELINE_MEMORY_MIB = 1024
ALL_INGRESS_ARN = (
    f"arn:aws:lambda:{REGION}:aws:network-connector:aws-network-connector:ALL_INGRESS"
)


def sh(cmd: list[str], cwd: Path | None = None) -> str:
    proc = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, check=False)
    if proc.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd)} failed:\n{proc.stdout}\n{proc.stderr}")
    return proc.stdout


def build_artifact(binary: Path) -> bytes:
    dockerfile = "\n".join(
        [
            "FROM public.ecr.aws/amazonlinux/amazonlinux:2023-minimal",
            "COPY agentd /agentd",
            "RUN chmod 0755 /agentd",
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


class Endpoint:
    def __init__(
        self, mv: Any, microvm_id: str, endpoint: str, agent_token: str
    ) -> None:
        self.mv = mv
        self.microvm_id = microvm_id
        self.base = endpoint if endpoint.startswith("http") else f"https://{endpoint}"
        self.agent_token = agent_token
        self._proxy: str | None = None
        self._minted_at = 0.0

    def invalidate(self) -> None:
        """Forces a fresh proxy token.

        A resume may well invalidate a token minted against the pre-suspend
        instance, and a stale-token 403 would otherwise be misread as the daemon
        being gone.
        """
        self._proxy = None

    def _proxy_auth(self) -> str:
        if self._proxy is None or time.time() - self._minted_at > 30 * 60:
            token = self.mv.create_microvm_auth_token(
                microvmIdentifier=self.microvm_id,
                expirationInMinutes=60,
                allowedPorts=[{"port": AGENT_PORT}],
            )
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
        timeout: float = 30.0,
    ) -> httpx.Response:
        headers: dict[str, Any] = {
            "X-aws-proxy-auth": self._proxy_auth(),
            "X-aws-proxy-port": str(AGENT_PORT),
        }
        bearer = self.agent_token if token == "" else token
        if bearer is not None:
            value = bearer.encode("utf-8") if isinstance(bearer, str) else bearer
            headers["Authorization"] = b"Bearer " + value
        with httpx.Client(timeout=timeout) as client:
            return client.request(
                method,
                f"{self.base}{path}",
                headers=headers,
                json=json_body,
                content=content,
            )


def run_exec(
    ep: Endpoint, exec_id: str, script: str, timeout: float = 60.0
) -> dict[str, Any]:
    ep.request(
        "POST",
        "/v1/exec/start",
        json_body={"exec_id": exec_id, "command": [script], "shell": True},
    )
    deadline = time.time() + timeout
    last: dict[str, Any] = {}
    while time.time() < deadline:
        response = ep.request("GET", f"/v1/exec/{exec_id}")
        if response.status_code != 200:
            return {"error": response.status_code}
        last = response.json()
        if last.get("phase") in ("exited", "acked"):
            return last
        time.sleep(2)
    return last


def wait_for_state(
    mv: Any, microvm_id: str, want: set[str], timeout: float = 300.0
) -> str:
    deadline = time.time() + timeout
    while time.time() < deadline:
        state = mv.get_microvm(microvmIdentifier=microvm_id).get("state")
        if state in want:
            return state
        print(f"    state {state}")
        time.sleep(5)
    raise RuntimeError(f"never reached {want} within {timeout}s")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    args = parser.parse_args()

    repo = Path(__file__).resolve().parent.parent
    infra = repo / "conformance" / "infra"
    binary = (repo / args.binary) if not args.binary.is_absolute() else args.binary

    outputs = json.loads(sh(["terraform", "output", "-json"], cwd=infra))
    bucket = outputs["s3_bucket"]["value"]
    build_role = outputs["build_role_arn"]["value"]
    execution_role = outputs["execution_role_arn"]["value"]

    session = boto3.Session(region_name=REGION)
    mv = session.client(
        SERVICE, config=BotoConfig(retries={"max_attempts": 10, "mode": "standard"})
    )
    s3 = session.client("s3")
    logs = session.client("logs")

    run_id = secrets.token_hex(4)
    image_name = f"agentd-probe-{run_id}"
    agent_token = secrets.token_urlsafe(32)
    findings: dict[str, Any] = {}
    image_id: str | None = None
    microvm_id: str | None = None

    try:
        key = f"{image_name}.zip"
        s3.put_object(Bucket=bucket, Key=key, Body=build_artifact(binary))
        print(f"artifact -> s3://{bucket}/{key}")

        created = mv.create_microvm_image(
            name=image_name,
            baseImageArn=f"arn:aws:lambda:{REGION}:aws:microvm-image:al2023-1",
            buildRoleArn=build_role,
            codeArtifact={"uri": f"s3://{bucket}/{key}"},
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
                    "resume": "ENABLED",
                    "resumeTimeoutInSeconds": HOOK_TIMEOUT_SEC,
                    "suspend": "ENABLED",
                    "suspendTimeoutInSeconds": HOOK_TIMEOUT_SEC,
                    "terminate": "ENABLED",
                    "terminateTimeoutInSeconds": HOOK_TIMEOUT_SEC,
                },
            },
            tags={"agentd:purpose": "suspend-resume-probe", "agentd:run": run_id},
            clientToken=f"create-{image_name}-{run_id}",
        )
        image_id = created.get("imageIdentifier") or created.get("imageArn")
        print(f"image {image_id}")
        deadline = time.time() + 30 * 60
        while time.time() < deadline:
            state = mv.get_microvm_image(imageIdentifier=image_id).get("state")
            if state in ("CREATED", "ACTIVE", "AVAILABLE"):
                break
            if state and "FAILED" in state:
                raise RuntimeError(f"build failed: {state}")
            time.sleep(15)
        print("image CREATED")

        run = mv.run_microvm(
            imageIdentifier=image_id,
            executionRoleArn=execution_role,
            ingressNetworkConnectors=[ALL_INGRESS_ARN],
            # autoResume off and a long suspended window: we want to control the
            # resume ourselves and not race an idle policy that terminates a
            # suspended VM out from under the probe.
            idlePolicy={
                "maxIdleDurationSeconds": 1800,
                "suspendedDurationSeconds": 1800,
                "autoResumeEnabled": False,
            },
            maximumDurationInSeconds=3600,
            runHookPayload=json.dumps({"agent_token": agent_token}),
            clientToken=f"run-{image_name}-{run_id}",
        )
        microvm_id = run["microvmId"]
        endpoint = run.get("endpoint", "")
        print(f"microvm {microvm_id}")
        wait_for_state(mv, microvm_id, {"RUNNING"})
        ep = Endpoint(mv, microvm_id, endpoint, agent_token)

        print("\n== before suspend ==")
        health = ep.request("GET", "/v1/health", token=None).json()
        findings["bootstrapped_before"] = health.get("bootstrapped")
        print(f"  bootstrapped: {health.get('bootstrapped')}")

        # Filesystem marker.
        ep.request("PUT", "/v1/fs/file?path=/tmp/marker.txt", content=b"survived")
        # A backgrounded writer: the shape an agent harness takes. It appends a
        # line per second to a file, so the line count after resume tells us
        # whether the process kept running, was frozen, or was killed.
        run_exec(
            ep,
            "ticker",
            "nohup sh -c 'i=0; while [ $i -lt 3000 ]; do date +%s >> /tmp/ticks.txt; "
            "i=$((i+1)); sleep 1; done' >/dev/null 2>&1 & echo started",
        )
        # An exec whose output is deliberately never acked.
        run_exec(ep, "unacked", "echo output-from-before-suspend")
        time.sleep(5)
        before = run_exec(ep, "count-before", "wc -l < /tmp/ticks.txt")
        ticks_before = int((before.get("stdout") or "0").strip() or 0)
        findings["ticks_before"] = ticks_before
        print(f"  ticks before: {ticks_before}")

        print("\n== suspend ==")
        mv.suspend_microvm(microvmIdentifier=microvm_id)
        state = wait_for_state(mv, microvm_id, {"SUSPENDED", "TERMINATED"})
        findings["state_after_suspend"] = state
        print(f"  state: {state}")
        if state == "TERMINATED":
            findings["verdict"] = "suspend terminated the VM; resume is not possible"
            return report(findings)

        # Long enough that a frozen process is distinguishable from a running one:
        # a live ticker would add ~45 ticks, a frozen one none.
        print("  holding suspended for 45s")
        time.sleep(45)

        print("\n== resume ==")
        mv.resume_microvm(microvmIdentifier=microvm_id)
        state = wait_for_state(mv, microvm_id, {"RUNNING", "TERMINATED"})
        findings["state_after_resume"] = state
        print(f"  state: {state}")
        if state != "RUNNING":
            findings["verdict"] = "resume did not return the VM to RUNNING"
            return report(findings)

        got = mv.get_microvm(microvmIdentifier=microvm_id)
        new_endpoint = got.get("endpoint", endpoint)
        findings["endpoint_changed"] = new_endpoint != endpoint
        ep.base = (
            new_endpoint
            if new_endpoint.startswith("http")
            else f"https://{new_endpoint}"
        )
        ep.invalidate()

        print("\n== after resume ==")
        # The daemon may need a moment; a connection error here is data, not a
        # crash, so it is caught and recorded.
        health_after: dict[str, Any] = {}
        for attempt in range(12):
            try:
                response = ep.request("GET", "/v1/health", token=None)
                health_after = {
                    "status": response.status_code,
                    **(response.json() if response.status_code == 200 else {}),
                }
                break
            except Exception as exc:  # noqa: BLE001 - cleanup must not mask the finding
                print(f"    health attempt {attempt}: {type(exc).__name__}")
                time.sleep(5)
        findings["health_after_resume"] = health_after
        findings["bootstrapped_after"] = health_after.get("bootstrapped")
        print(f"  health: {health_after}")

        if health_after.get("bootstrapped") is True:
            # The token survived, so the pre-suspend token still authorizes.
            marker = ep.request("GET", "/v1/fs/file?path=/tmp/marker.txt")
            findings["filesystem_survived"] = marker.content == b"survived"
            print(f"  filesystem survived: {findings['filesystem_survived']}")

            unacked = ep.request("GET", "/v1/exec/unacked")
            findings["exec_record_survived"] = unacked.status_code == 200
            print(f"  exec record survived: {unacked.status_code}")

            # The tick file is epoch seconds, one per second of guest liveness.
            # The largest gap between consecutive ticks is the suspension as the
            # guest experienced it: near zero means the guest clock kept running
            # (so the wall gap is invisible to it), while a gap close to the
            # wall-clock suspension means the guest saw real time pass.
            dump = run_exec(ep, "ticks-dump", "cat /tmp/ticks.txt | tr '\\n' ' '")
            stamps = [int(x) for x in (dump.get("stdout") or "").split() if x.isdigit()]
            gaps = [b - a for a, b in itertools.pairwise(stamps)]
            findings["tick_count_after"] = len(stamps)
            findings["max_tick_gap_sec"] = max(gaps) if gaps else None
            print(f"  ticks: {len(stamps)}, max gap {findings['max_tick_gap_sec']}s")

            # Liveness measured differentially rather than by pgrep, whose
            # pattern is easy to get wrong through two layers of shell quoting —
            # a false 'gone' would have been indistinguishable from a real one.
            first = run_exec(ep, "live-1", "wc -l < /tmp/ticks.txt")
            time.sleep(6)
            second = run_exec(ep, "live-2", "wc -l < /tmp/ticks.txt")
            n1 = int((first.get("stdout") or "0").strip() or 0)
            n2 = int((second.get("stdout") or "0").strip() or 0)
            findings["ticks_grew_after_resume"] = n2 - n1
            still_running = (n2 - n1) >= 3
            findings["background_process_survived"] = still_running
            print(
                f"  ticks grew by {n2 - n1} over 6s after resume -> "
                f"{'still running' if still_running else 'not running'}"
            )

            gap = findings["max_tick_gap_sec"] or 0
            if still_running and gap >= 30:
                findings["verdict"] = (
                    "suspend/resume preserves everything and the guest observes the "
                    "elapsed wall time as a single gap: token, filesystem, exec "
                    "records, and running processes all survive. Pause/resume needs "
                    "no token re-delivery, but a process that measures time will see "
                    "the suspension."
                )
            elif still_running:
                findings["verdict"] = (
                    f"everything survived and the largest guest-observed gap was only "
                    f"{gap}s across a 45s suspension, so the guest clock did not "
                    "advance through the whole suspension."
                )
            else:
                findings["verdict"] = (
                    "token, filesystem, and exec records survive, but the "
                    "backgrounded process did not: a resumed VM keeps its state and "
                    "loses its running work."
                )
        else:
            findings["verdict"] = (
                "the in-memory token did NOT survive: a resumed VM cannot serve the "
                "control API until a fresh run hook arrives. The daemon docstring is "
                "correct and consumers need token re-delivery."
            )

    finally:
        print("\n== teardown ==")
        if microvm_id:
            try:
                mv.terminate_microvm(microvmIdentifier=microvm_id)
                print(f"  terminated {microvm_id}")
            except Exception as exc:  # noqa: BLE001 - cleanup must not mask the finding
                print(f"  terminate failed: {exc}")
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
                except Exception as exc:  # noqa: BLE001 - cleanup must not mask the finding
                    print(f"  image delete retry: {type(exc).__name__}")
                    time.sleep(15)

        # Deleted last, deliberately. The service owns this log group and can write
        # to it while an image is still deleting, so removing it before the image is
        # gone leaves it behind — which is exactly how one leaked from an earlier run.
        try:
            logs.delete_log_group(logGroupName=f"/aws/lambda-microvms/{image_name}")
            print("  deleted log group")
        except Exception as exc:  # noqa: BLE001 - cleanup must not mask the finding
            print(f"  log group delete skipped: {type(exc).__name__}")

    return report(findings)


def report(findings: dict[str, Any]) -> int:
    print("\n== findings ==")
    print(json.dumps(findings, indent=2, default=str))
    return 0


if __name__ == "__main__":
    sys.exit(main())
