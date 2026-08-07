#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["microvms-agentd-client", "boto3>=1.40", "httpx>=0.27"]
#
# [tool.uv.sources]
# microvms-agentd-client = { path = "../clients/python" }
# ///
"""Measure what a caller can learn when memory runs out inside a MicroVM.

The question a customer actually asks: "is there a `dmesg` for this?" It has two
answers, because two very different things get called an OOM.

A *process* killed inside a living VM should be visible twice over — as a signal
on the exec result, and in the guest's own kernel ring buffer. That is checkable
and this probe checks it.

The *VM itself* dying is the case nobody has measured. `GetMicrovm` carries a
`stateReason` string documented only as "the reason for why the MicroVM is in the
current state", and we know it carries real text because a failed run hook once
produced "Run lifecycle hook returned HTTP status 400". Whether guest-wide memory
exhaustion populates it, and with what wording, is the gap this probe closes.

The escalation matters more than any single case, so there are four:

  1. A greedy child on a VM with room to spare. Expect the guest OOM killer to
     take the child and the VM to survive — signal 9 on the exec, a line in
     dmesg, the daemon still answering.
  2. The same, at the smallest baseline the platform offers. The interesting
     variable is whether a tighter budget changes *who* the kernel picks.
  3. Sustained pressure from several children at once. This is the shape an agent
     harness produces (parallel test workers), and it is where the kernel's
     choice of victim stops being obvious.
  4. Whether the daemon itself can be the victim. If it is, the VM becomes
     unreachable while still billing — the failure this project designed hardest
     against — and `stateReason` is the only channel left.

Every finding lands in docs/PLATFORM.md with this run's date and region. Where a
case does not reproduce, that is recorded too: "we could not make the platform do
this" is a fact a reader can use.
"""

from __future__ import annotations

import argparse
import json
import os
import secrets
import subprocess
import sys
from pathlib import Path
from typing import Any

import boto3
from microvms_agentd import (
    AgentdError,
    Sandbox,
    Session,
    default_dockerfile,
    default_hooks,
)

REGION = os.environ.get("AWS_REGION", "us-east-1")
AGENT_PORT = 9000
HOOK_TIMEOUT_SEC = 30
# The smallest baseline the platform offers, and the sharpest test of the memory
# bounds. Harbor's provider settled on 8 GiB as its floor; the daemon is happy in
# far less, and less is where the kernel's choices get interesting.
SMALL_MEMORY_MIB = 512
ROOMY_MEMORY_MIB = 2048


def sh(cmd: list[str], cwd: Path | None = None) -> str:
    proc = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, check=False)
    if proc.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd)} failed:\n{proc.stdout}\n{proc.stderr}")
    return proc.stdout


def run_shell(
    session: Session, exec_id: str, script: str, timeout: float = 120.0
) -> Any:
    """Runs a shell command and returns its completed result, or the error."""
    try:
        session.run(script, shell=True, exec_id=exec_id)
        return session.exec(exec_id).wait(timeout=timeout, interval=2)
    except AgentdError as exc:
        return exc


def describe(result: Any) -> dict[str, Any]:
    """Flattens an exec result (or an error) into something JSON can hold."""
    if isinstance(result, AgentdError):
        return {"error": type(result).__name__, "detail": str(result)[:200]}
    return {
        "exit_code": result.exit_code,
        # The load-bearing field: a process the kernel killed reports a signal
        # rather than an exit code, and SIGKILL (9) is what the OOM killer sends.
        "signal": result.signal,
        "truncated": result.truncated,
        "stdout_tail": (result.stdout or "")[-400:],
        "stderr_tail": (result.stderr or "")[-400:],
    }


def guest_oom_evidence(session: Session, tag: str) -> dict[str, Any]:
    """Asks the guest kernel what it did, which is the `dmesg` the customer wants."""
    evidence: dict[str, Any] = {}

    # The classic OOM report. `dmesg` needs no privilege to read on a stock
    # al2023 guest, but a failure here is data rather than an error: it would mean
    # the answer to "is there a dmesg" is no.
    dmesg = run_shell(
        session,
        f"dmesg-{tag}",
        "dmesg 2>&1 | grep -iE 'killed process|out of memory|oom' | tail -20 || echo NO_DMESG_MATCH",
    )
    evidence["dmesg"] = describe(dmesg)

    # The cgroup-level counter, which is how a supervisor would poll for pressure
    # rather than discover it after the fact.
    counters = run_shell(
        session,
        f"oomcount-{tag}",
        "cat /sys/fs/cgroup/memory.events 2>/dev/null || cat /sys/fs/cgroup/memory/memory.oom_control 2>/dev/null || echo NO_CGROUP_EVENTS",
    )
    evidence["cgroup_events"] = describe(counters)

    meminfo = run_shell(
        session,
        f"meminfo-{tag}",
        "grep -E 'MemTotal|MemAvailable|SwapTotal' /proc/meminfo",
    )
    evidence["meminfo"] = describe(meminfo)
    return evidence


def platform_view(mv: Any, microvm_id: str) -> dict[str, Any]:
    """What the control plane says, which is all a caller has if the VM is gone."""
    try:
        got = mv.get_microvm(microvmIdentifier=microvm_id)
    except Exception as exc:  # noqa: BLE001 - the answer includes "we could not ask"
        return {"error": type(exc).__name__, "detail": str(exc)[:200]}
    return {
        "state": got.get("state"),
        # The whole question for the VM-death case.
        "stateReason": got.get("stateReason"),
        "terminatedAt": str(got.get("terminatedAt"))
        if got.get("terminatedAt")
        else None,
    }


def case_greedy_child(
    session: Session, mv: Any, microvm_id: str, mib: int
) -> dict[str, Any]:
    """One child asks for far more than the VM has. Who dies?"""
    findings: dict[str, Any] = {"memory_mib": mib}

    # Allocating with `dd` into a tmpfs-backed file would hit the disk guard
    # instead, so this touches anonymous memory directly. `tr` holds its output
    # in a pipe the reader never drains fast enough, which is a realistic shape:
    # it is what a build log or a chatty test suite does.
    hog = run_shell(
        session,
        f"hog-{mib}",
        # Ask for roughly four times the VM's whole budget, in one process.
        # The minimal base image has no python3 — the first run of this probe used
        # it and measured nothing, reporting "command not found" as though the
        # platform had survived a test it never ran. /dev/shm is tmpfs on a stock
        # guest, so a dd into it is anonymous memory the kernel must really back,
        # with no interpreter to be missing.
        f"dd if=/dev/zero of=/dev/shm/hog bs=1M count={mib * 2} 2>&1 | tail -2; rm -f /dev/shm/hog",
        timeout=180,
    )
    findings["hog"] = describe(hog)

    # Did the VM survive its own OOM killer? This is the difference between an
    # annoying failed command and an unreachable sandbox.
    try:
        health = session.health()
        findings["daemon_after"] = {
            "reachable": True,
            "bootstrapped": health.bootstrapped,
        }
    except AgentdError as exc:
        findings["daemon_after"] = {"reachable": False, "error": type(exc).__name__}

    findings["guest_evidence"] = guest_oom_evidence(session, f"g{mib}")
    findings["platform_view"] = platform_view(mv, microvm_id)
    return findings


def case_parallel_pressure(
    session: Session, mv: Any, microvm_id: str
) -> dict[str, Any]:
    """Several children at once — the shape a parallel test suite produces."""
    findings: dict[str, Any] = {}
    swarm = run_shell(
        session,
        "swarm",
        # Six workers each asking for a third of the budget: collectively far over,
        # individually plausible. The kernel has to choose, and which one it picks
        # is the thing a harness author cannot predict.
        "for i in 1 2 3 4 5 6; do (dd if=/dev/zero of=/dev/shm/w$i bs=1M count=250 2>/dev/null; sleep 15) & done; wait; echo done; rm -f /dev/shm/w*",
        timeout=180,
    )
    findings["swarm"] = describe(swarm)
    try:
        findings["daemon_after"] = {
            "reachable": True,
            "bootstrapped": session.health().bootstrapped,
        }
    except AgentdError as exc:
        findings["daemon_after"] = {"reachable": False, "error": type(exc).__name__}
    findings["guest_evidence"] = guest_oom_evidence(session, "swarm")
    findings["platform_view"] = platform_view(mv, microvm_id)
    return findings


def case_daemon_as_victim(session: Session, mv: Any, microvm_id: str) -> dict[str, Any]:
    """Can the daemon itself be chosen? If so the VM is lost, and this says how it looks.

    Deliberately last, because it may end the VM. The mechanism is indirect on
    purpose: rather than attacking the daemon, this asks it to hold a large
    response while memory is scarce, which is the realistic path — the daemon's
    buffers are bounded, so the interesting question is whether the *kernel*
    picks it anyway when a child is greedier.
    """
    findings: dict[str, Any] = {}
    # A command whose output exceeds the daemon's own output cap while memory is
    # already tight. The cap should hold and the daemon should truncate rather
    # than grow — that guard exists precisely for this.
    noisy = run_shell(
        session,
        "noisy-under-pressure",
        "(dd if=/dev/zero of=/dev/shm/pressure bs=1M count=400 2>/dev/null) & sleep 1; dd if=/dev/zero bs=1M count=64 2>/dev/null | tr '\\0' 'y'; wait; rm -f /dev/shm/pressure",
        timeout=180,
    )
    findings["noisy_under_pressure"] = describe(noisy)
    try:
        health = session.health()
        findings["daemon_after"] = {
            "reachable": True,
            "bootstrapped": health.bootstrapped,
            "disk_available_bytes": health.available_bytes,
        }
    except AgentdError as exc:
        findings["daemon_after"] = {"reachable": False, "error": type(exc).__name__}
    findings["guest_evidence"] = guest_oom_evidence(session, "victim")
    findings["platform_view"] = platform_view(mv, microvm_id)
    return findings


def probe_one_vm(box: Sandbox, mv: Any, mib: int, *, parallel: bool) -> dict[str, Any]:
    """Builds an image, runs a VM at `mib`, and measures. Always tears down."""
    run_id = secrets.token_hex(4)
    image_name = f"agentd-probe-oom{mib}-{run_id}"
    findings: dict[str, Any] = {"memory_mib": mib}

    repo = Path(__file__).resolve().parent.parent
    infra = repo / "conformance" / "infra"
    outputs = json.loads(sh(["terraform", "output", "-json"], cwd=infra))

    try:
        print(f"\n== {mib} MiB VM ==")
        box.build_image(
            name=image_name,
            binary=repo / "target/aarch64-unknown-linux-musl/release/agentd",
            bucket=outputs["s3_bucket"]["value"],
            build_role_arn=outputs["build_role_arn"]["value"],
            dockerfile=default_dockerfile(port=AGENT_PORT),
            memory_mib=mib,
            hooks=default_hooks(AGENT_PORT, HOOK_TIMEOUT_SEC),
            tags={"agentd:purpose": "oom-probe"},
            os_capabilities=["ALL"],
            client_token=f"create-{image_name}-{run_id}",
        )
        print("  image CREATED")

        session = box.run(
            execution_role_arn=outputs["execution_role_arn"]["value"],
            agent_token=secrets.token_urlsafe(32),
            max_idle_sec=900,
            suspended_sec=900,
            auto_resume=False,
            max_duration_sec=1800,
            client_token=f"run-{image_name}-{run_id}",
        )
        print(f"  microvm RUNNING ({box.microvm_id})")

        findings["baseline_meminfo"] = describe(
            run_shell(
                session, "baseline", "grep -E 'MemTotal|MemAvailable' /proc/meminfo"
            )
        )
        findings["greedy_child"] = case_greedy_child(session, mv, box.microvm_id, mib)
        if parallel:
            findings["parallel_pressure"] = case_parallel_pressure(
                session, mv, box.microvm_id
            )
            findings["daemon_as_victim"] = case_daemon_as_victim(
                session, mv, box.microvm_id
            )

    except Exception as exc:  # noqa: BLE001 - a failure is a finding, not a crash
        findings["probe_error"] = {"type": type(exc).__name__, "detail": str(exc)[:400]}
        if box.microvm_id:
            findings["platform_view_after_error"] = platform_view(mv, box.microvm_id)
    finally:
        print("  tearing down")
        try:
            box.terminate(delete_image=True, delete_log_group=True)
        except Exception as exc:  # noqa: BLE001 - teardown must not mask the finding
            print(f"  teardown issue: {type(exc).__name__}")

    return findings


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--memory",
        type=int,
        action="append",
        help=f"baseline sizes to probe (default: {SMALL_MEMORY_MIB} and {ROOMY_MEMORY_MIB})",
    )
    args = parser.parse_args()
    sizes = args.memory or [SMALL_MEMORY_MIB, ROOMY_MEMORY_MIB]

    session = boto3.Session(region_name=REGION)
    mv = session.client("lambda-microvms")

    report: dict[str, Any] = {"region": REGION, "sizes": sizes, "results": []}
    for mib in sizes:
        box = Sandbox(region=REGION)
        # The full escalation only on the smallest VM: that is where the kernel's
        # choices are tightest, and each extra case costs a live minute.
        result = probe_one_vm(box, mv, mib, parallel=(mib == min(sizes)))
        report["results"].append(result)

    print("\n== findings ==")
    print(json.dumps(report, indent=2, default=str))
    return 0


if __name__ == "__main__":
    sys.exit(main())
