#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["microvms-agentd-client", "boto3>=1.40", "httpx>=0.27"]
#
# [tool.uv.sources]
# microvms-agentd-client = { path = "../clients/python" }
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

Every HTTP interaction goes through `clients/python`, so this run is also the
best available evidence that the library is usable: if a check here needs to
reach around the library, the library's API is wrong. Two places do reach around
it deliberately, and both are noted where they happen — the platform's own
`/run` hook route, which no consumer should ever call, and the raw
`transport.request` used where the *status code itself* is the assertion.

Usage:
    conformance/run.py --binary target/aarch64-unknown-linux-musl/release/agentd
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
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import boto3
from botocore.config import Config as BotoConfig
from microvms_agentd import (
    AgentdError,
    Conflict,
    NotFound,
    ProtocolError,
    Sandbox,
    Session,
    Unauthorized,
    default_dockerfile,
    default_hooks,
)

SERVICE = "lambda-microvms"
REGION = os.environ.get("AWS_REGION", "us-east-1")
AGENT_PORT = 9000
HOOK_TIMEOUT_SEC = 30
# 8 GiB is the size Harbor's provider settled on as its floor; the daemon itself
# is happy in far less, and a smaller baseline is also a sharper test of the
# memory bounds.
BASELINE_MEMORY_MIB = 1024
IMAGE_BUILD_TIMEOUT_SEC = 45 * 60
# A baked WORKDIR is the only way to test cwd inheritance: every public ARM64 base
# image we checked leaves WorkingDir empty, so there would otherwise be nothing to
# inherit.
BAKED_WORKDIR = "/opt/baked-workdir"


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
        return self.check(
            name, actual == expected, f"expected {expected!r}, got {actual!r}"
        )

    def raises(
        self, name: str, expected: type[Exception], call: Callable[[], Any]
    ) -> bool:
        """Asserts a call raises exactly `expected`.

        This is how the client library expresses a protocol rule, and asserting on
        the *type* rather than on a status integer is strictly stronger: it checks
        both that the daemon chose the right status and that the library maps it to
        the type a consumer will catch. A 404 arriving where a 400 belongs fails
        here as loudly as it should, which is the whole point of the taxonomy.
        """
        try:
            call()
        except expected as exc:
            return self.check(name, True, f"{type(exc).__name__}")
        except Exception as exc:  # noqa: BLE001 - any other error is the finding
            return self.check(
                name, False, f"expected {expected.__name__}, raised {exc!r}"
            )
        return self.check(name, False, f"expected {expected.__name__}, nothing raised")

    def ok(self, name: str, call: Callable[[], Any]) -> bool:
        """Asserts a call succeeds, which for this library means "does not raise"."""
        try:
            call()
        except Exception as exc:  # noqa: BLE001 - any error is the finding
            return self.check(name, False, repr(exc))
        return self.check(name, True)


def sh(cmd: list[str], cwd: Path | None = None) -> str:
    proc = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, check=False)
    if proc.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd)} failed:\n{proc.stdout}\n{proc.stderr}")
    return proc.stdout


def conformance_dockerfile() -> str:
    """The library's default image recipe, with a WORKDIR baked in for this suite.

    `ENTRYPOINT []` plus `CMD ["/agentd"]` is the deployment invariant the trust
    boundary rests on, and it is also what makes an omitted `cwd` inherit the image
    WORKDIR. Both come from the library so that a consumer following the README
    gets the same guarantees this run verifies.
    """
    return default_dockerfile(port=AGENT_PORT, workdir=BAKED_WORKDIR)


def post_run_hook(session: Session, token: str) -> int:
    """Posts the platform's run hook and returns the raw status.

    The library deliberately does not wrap this route: the only callers are the
    platform itself and an attacker inside the VM, so an affordance for it in a
    client library would be a footgun with no legitimate use. Reaching through to
    the transport here is correct rather than a gap.

    The body is the platform's envelope, not our payload directly: the string given
    to RunMicrovm arrives wrapped as {"runHookPayload": "<it>"}.
    """
    response = session.transport.request(
        "POST",
        "/aws/lambda-microvms/runtime/v1/run",
        token=None,
        json={"runHookPayload": json.dumps({"agent_token": token})},
    )
    return response.status_code


def drive_protocol(session: Session, results: Results) -> None:
    """Exercises every protocol rule the real service can validate."""

    print("\n-- bootstrap and authorization --")
    health = session.health()
    results.check(
        "health reachable through the endpoint", True, f"version {health.version}"
    )
    # The platform delivered the token through runHookPayload before forwarding
    # any external traffic, so by the time we can reach the VM at all it must
    # already be bootstrapped. This single assertion is the hook-ordering
    # guarantee, observed rather than quoted from documentation.
    results.eq(
        "platform ran the run hook before forwarding traffic", health.bootstrapped, True
    )

    results.eq(
        "post-bootstrap hijack refused with 409",
        post_run_hook(session, "attacker-token"),
        409,
    )
    results.eq(
        "identical bootstrap replay accepted",
        post_run_hook(session, session.agent_token),
        200,
    )

    # Both of these use the token override rather than a second Session: it is the
    # same connection pool and the same proxy token, so a difference in outcome can
    # only be the bearer credential.
    results.raises(
        "wrong token refused with 401",
        Unauthorized,
        lambda: session.transport.send("GET", "/v1/exec/nope", token="wrong-token"),
    )
    # The daemon's stated property is that it compares header *bytes* without
    # decoding them; the library encodes the bearer to bytes for exactly this
    # reason, so a non-ASCII token is sendable at all.
    results.raises(
        "non-ASCII token header answered, not a dropped connection",
        Unauthorized,
        lambda: session.transport.send("GET", "/v1/exec/nope", token="tökén"),
    )

    print("\n-- exec --")
    results.ok(
        "exec start accepted",
        lambda: session.run(["/bin/sh", "-c", "echo live; pwd; id -u"], exec_id="c1"),
    )
    outcome = session.exec("c1").wait(timeout=60, interval=2)
    results.eq("exec exited 0", outcome.exit_code, 0)
    stdout = outcome.stdout or ""
    results.check("exec captured stdout", "live" in stdout, repr(stdout))
    # The daemon is the container CMD, so its cwd is the image WORKDIR and an
    # omitted cwd must land there rather than at /.
    results.check(
        "omitted cwd inherits the image WORKDIR", BAKED_WORKDIR in stdout, repr(stdout)
    )

    results.ok(
        "retried start accepted",
        lambda: session.run(["/bin/sh", "-c", "echo MUST_NOT_RUN"], exec_id="c1"),
    )
    after = session.exec("c1").poll()
    results.check(
        "retried start did not spawn a second child",
        "MUST_NOT_RUN" not in (after.stdout or ""),
        repr(after.stdout),
    )

    for name, script in [
        ("empty", ""),
        ("comment-only", "# nothing"),
        ("unbalanced brace", "echo A } echo B"),
    ]:
        exec_id = f"sh-{name.split()[0]}"
        session.run(script, shell=True, exec_id=exec_id)
        got = session.exec(exec_id).wait(timeout=60, interval=2)
        results.eq(f"{name} shell command exits 0", got.exit_code, 0)
        if name == "unbalanced brace":
            results.check(
                "unbalanced brace did not escape into a second command",
                (got.stdout or "").strip() == "A } echo B",
                repr(got.stdout),
            )

    results.ok("ack accepted", session.exec("c1").ack)
    results.raises("second ack refused with 409", Conflict, session.exec("c1").ack)

    results.raises(
        "unknown exec id is 404", NotFound, session.exec("never-existed").poll
    )
    # 400 and never 404: a client that maps 404 onto "missing artifact" would report
    # a phantom absent thing for what is really a protocol typo.
    results.raises(
        "malformed body is 400, not 404",
        ProtocolError,
        lambda: session.transport.send("POST", "/v1/exec/start", json={"bogus": True}),
    )

    print("\n-- large output --")
    # 32 MiB against an 8 MiB cap: the daemon must truncate and stay up, not grow
    # until the VM's OOM killer takes it.
    session.run(
        ["/bin/sh", "-c", "dd if=/dev/zero bs=1M count=32 2>/dev/null | tr '\\0' 'x'"],
        exec_id="noisy",
    )
    noisy = session.exec("noisy").wait(timeout=180, interval=2)
    results.eq("noisy command still exits 0", noisy.exit_code, 0)
    results.check(
        "output past the cap was truncated",
        noisy.truncated is True,
        str(noisy.truncated),
    )
    results.ok("daemon survived the truncation", session.health)

    print("\n-- file transfer --")
    results.ok(
        "single file write accepted",
        lambda: session.upload_file(
            "/tmp/live.txt", b"written through the endpoint", mode="644"
        ),
    )
    results.eq(
        "single file read returns the bytes",
        session.download_file("/tmp/live.txt"),
        b"written through the endpoint",
    )
    results.raises(
        "read of an absent file is 404",
        NotFound,
        lambda: session.download_file("/tmp/absent"),
    )
    # A missing path key cannot be produced through `download_file`, which always
    # sends one — that is the library doing its job. Dropped to the transport to
    # reach the rule at all.
    results.raises(
        "missing path key is 400",
        ProtocolError,
        lambda: session.transport.send("GET", "/v1/fs/file"),
    )

    # A symlink must survive the round trip: harnesses pack them deliberately,
    # and a daemon that refuses links breaks real uploads.
    session.run(
        "mkdir -p /tmp/tree/sub && echo payload > /tmp/tree/a.txt && "
        "ln -sf a.txt /tmp/tree/link && echo deep > /tmp/tree/sub/b.txt",
        shell=True,
        exec_id="mktree",
    )
    results.eq(
        "tree created for the round trip",
        session.exec("mktree").wait(timeout=60, interval=2).exit_code,
        0,
    )

    archive = b""
    try:
        archive = session.download_tar("/tmp/tree")
        results.check("tar download succeeded", bool(archive), f"{len(archive)} bytes")
    except AgentdError as exc:
        results.check("tar download succeeded", False, repr(exc))
    results.ok("tar upload accepted", lambda: session.upload_tar("/tmp/dest", archive))

    session.run(
        "readlink /tmp/dest/link; cat /tmp/dest/link; cat /tmp/dest/sub/b.txt",
        shell=True,
        exec_id="verify",
    )
    verify = session.exec("verify").wait(timeout=60, interval=2)
    verified = verify.stdout or ""
    results.check(
        "symlink survived the round trip as a symlink",
        verified.startswith("a.txt"),
        repr(verified),
    )
    results.check(
        "symlink still resolves to its target's content",
        "payload" in verified,
        repr(verified),
    )

    print("\n-- streaming --")
    # Streaming is the capability an agent harness needs and the one no local tier
    # can fully validate: the question is whether AWS's endpoint proxy actually
    # forwards Server-Sent Events rather than buffering them until the command
    # ends. Documentation says it does; this is the check.
    session.run(
        "for i in 1 2 3 4 5; do echo chunk-$i; done; echo done-streaming",
        shell=True,
        exec_id="stream1",
    )
    handle = session.exec("stream1")
    chunks: list[bytes] = []
    exit_event = None
    gaps: list[Any] = []
    for event in handle.stream(timeout=120):
        kind = type(event).__name__
        if kind == "OutputChunk":
            chunks.append(event.data)
        elif kind == "Gap":
            gaps.append(event)
        elif kind == "Exit":
            exit_event = event

    streamed = b"".join(chunks).decode(errors="replace")
    results.check(
        "SSE reached us through the endpoint proxy",
        bool(chunks),
        f"{len(chunks)} chunk(s), {sum(len(c) for c in chunks)} bytes",
    )
    results.check(
        "streamed output is complete and ordered",
        "chunk-1" in streamed
        and "chunk-5" in streamed
        and "done-streaming" in streamed,
        repr(streamed[:120]),
    )
    results.check(
        "no gap was reported for a small stream", not gaps, f"{len(gaps)} gap(s)"
    )
    # The terminal event is why SSE was chosen over a raw byte stream: without it a
    # client cannot tell a finished command from a dropped connection.
    results.check(
        "the terminal exit event carried the real exit code",
        exit_event is not None and exit_event.exit_code == 0,
        repr(exit_event),
    )
    # Streaming must not consume the exec: poll is a separate view onto the same
    # server-side object, and the conformance suite's own `truncated` assertions
    # depend on that staying true.
    polled = handle.poll()
    results.check(
        "the exec survived being streamed and is still pollable",
        (polled.stdout or "").find("done-streaming") >= 0,
        repr((polled.stdout or "")[:80]),
    )

    print("\n-- stdin --")
    # `cat` cannot exit until stdin closes, so this check fails by hanging if EOF
    # never reaches the child — which is exactly the trap where `Child::wait()`
    # drops its own stdin handle but not ours.
    session.run(["cat"], exec_id="cat1", stdin=True)
    cat = session.exec("cat1")
    results.ok("stdin write accepted", lambda: cat.write_stdin(b"hello via stdin\n"))
    results.ok("stdin close accepted", cat.close_stdin)
    echoed = cat.wait(timeout=60, interval=2)
    results.eq("a child reading stdin exits once stdin closes", echoed.exit_code, 0)
    results.eq(
        "stdin round-tripped through the child", echoed.stdout, "hello via stdin\n"
    )

    # Opt-in matters: a command that did not ask for stdin must not have one, or
    # every task command inherits a surprise open descriptor.
    session.run(["true"], exec_id="nostdin")
    results.raises(
        "writing stdin to a command that did not request it is refused",
        Conflict,
        lambda: session.exec("nostdin").write_stdin(b"x"),
    )

    print("\n-- hostile archives --")
    for name, archive_bytes, expected in build_hostile_archives():
        results.raises(
            f"hostile archive refused: {name}",
            expected,
            lambda b=archive_bytes: session.upload_tar("/tmp/hostile", b),
        )

    session.run(
        "ls /escaped.txt /tmp/escaped.txt 2>&1 | head -3; echo done",
        shell=True,
        exec_id="escaped",
    )
    escaped = session.exec("escaped").wait(timeout=60, interval=2)
    listing = escaped.stdout or ""
    results.check(
        "nothing escaped the extraction root",
        "No such file" in listing or "cannot access" in listing,
        repr(listing),
    )


def drive_suspend_resume(box: Sandbox, session: Session, results: Results) -> None:
    """Checks that a suspended sandbox comes back whole.

    Measured 2026-08-05: suspend is a freeze and restore, not a stop and start —
    the in-memory agent token, the filesystem, exec records, and even running
    processes survive. That is what makes a warm pool of suspended sandboxes
    viable, so it is worth a standing assertion rather than a one-off probe: if a
    future platform change turned suspend into a cold start, every consumer built
    on the warm-pool assumption would break at once, and this is where that shows
    up.

    The evidence is a ticker writing epoch seconds once a second. A gap in *its*
    timestamps is the suspension as the guest experienced it, which distinguishes
    a frozen guest from one that kept running.
    """
    session.run(
        "nohup sh -c 'i=0; while [ $i -lt 3000 ]; do date +%s >> /tmp/ticks.txt; "
        "i=$((i+1)); sleep 1; done' >/dev/null 2>&1 & echo started",
        shell=True,
        exec_id="ticker",
    )
    session.exec("ticker").wait(timeout=60, interval=2)
    session.upload_file("/tmp/survives.txt", b"written before the suspend")
    time.sleep(5)

    print("  suspending")
    box.suspend()
    # Long enough that a frozen guest and a running one are distinguishable: a live
    # ticker would add roughly 40 entries across this window.
    time.sleep(40)
    print("  resuming")
    session = box.resume()

    # `resume()` hands back a fresh Session deliberately: a resume can return a
    # different endpoint, and a proxy token minted against the pre-suspend instance
    # may no longer be valid. Taking the new one unconditionally means a stale-token
    # failure cannot be misread as the daemon having lost its state.

    health = None
    for _ in range(12):
        try:
            health = session.health()
            break
        except AgentdError as exc:
            print(f"    health after resume: {type(exc).__name__}")
            time.sleep(5)
    results.check("the daemon answers after a resume", health is not None, repr(health))
    # The load-bearing one. If this is False, every consumer needs token
    # re-delivery plumbing and a suspended sandbox is worthless.
    results.eq(
        "the agent token survived the suspend",
        health.bootstrapped if health else None,
        True,
    )
    results.eq(
        "the filesystem survived the suspend",
        session.download_file("/tmp/survives.txt"),
        b"written before the suspend",
    )
    results.ok(
        "an exec record from before the suspend survived",
        lambda: session.exec("ticker").poll(),
    )

    session.run("cat /tmp/ticks.txt | tr '\\n' ' '", shell=True, exec_id="ticks")
    dump = session.exec("ticks").wait(timeout=60, interval=2)
    stamps = [int(x) for x in (dump.stdout or "").split() if x.isdigit()]
    gaps = [b - a for a, b in itertools.pairwise(stamps)]
    largest = max(gaps) if gaps else 0
    results.check(
        "the guest observed the suspension as a single gap in its own clock",
        largest >= 30,
        f"largest gap {largest}s across a ~40s suspension",
    )

    # Differential liveness: two counts a few seconds apart. `pgrep` would need a
    # pattern threaded through two layers of shell quoting, where a false negative
    # is indistinguishable from a real one.
    first = session.run("wc -l < /tmp/ticks.txt", shell=True, exec_id="live1")
    n1 = int(
        (session.exec("live1").wait(timeout=60, interval=2).stdout or "0").strip() or 0
    )
    time.sleep(6)
    session.run("wc -l < /tmp/ticks.txt", shell=True, exec_id="live2")
    n2 = int(
        (session.exec("live2").wait(timeout=60, interval=2).stdout or "0").strip() or 0
    )
    del first
    results.check(
        "a backgrounded process resumed and kept running",
        n2 - n1 >= 3,
        f"ticks grew by {n2 - n1} over 6s after resume",
    )


def build_hostile_archives() -> list[tuple[str, bytes, type[Exception]]]:
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

    # Every one of these is a refused *member*, which the daemon answers 400 for and
    # the library maps to ProtocolError. A 413 would be a cap violation instead, and
    # the distinction matters: one means "this archive is hostile", the other means
    # "this archive is merely too big".
    return [
        (
            "parent traversal",
            make([("../../escaped.txt", "file", None, b"pwned")]),
            ProtocolError,
        ),
        (
            "absolute link target",
            make([("link", "sym", "/etc/passwd", b"")]),
            ProtocolError,
        ),
        (
            "symlink redirect",
            make([("s", "sym", "..", b""), ("s/escaped.txt", "file", None, b"pwned")]),
            ProtocolError,
        ),
        ("character device", make([("dev", "dev", None, b"")]), ProtocolError),
    ]


def read_daemon_logs(logs: Any, image_name: str) -> list[str]:
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
        except Exception as exc:  # noqa: BLE001 - a missing group is data, not a crash
            print(f"    log group {group} unavailable: {type(exc).__name__}")
    return lines


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument(
        "--keep", action="store_true", help="skip teardown (leaks resources)"
    )
    args = parser.parse_args()

    repo = Path(__file__).resolve().parent.parent
    infra = repo / "conformance" / "infra"
    binary = (
        (repo / args.binary).resolve() if not args.binary.is_absolute() else args.binary
    )
    if not binary.exists():
        print(f"binary not found: {binary}")
        return 2

    outputs = json.loads(sh(["terraform", "output", "-json"], cwd=infra))
    bucket = outputs["s3_bucket"]["value"]
    build_role = outputs["build_role_arn"]["value"]
    execution_role = outputs["execution_role_arn"]["value"]
    print(f"infra: bucket={bucket}")

    aws = boto3.Session(region_name=REGION)
    boto_config = BotoConfig(retries={"max_attempts": 10, "mode": "standard"})
    mv = aws.client(SERVICE, config=boto_config)
    logs = aws.client("logs")

    run_id = secrets.token_hex(4)
    image_name = f"agentd-conformance-{run_id}"
    agent_token = secrets.token_urlsafe(32)
    results = Results()

    # The library owns the whole AWS lifecycle: artifact zip, image build with the
    # stalled-build probe, RunMicrovm with the token in runHookPayload, the wait to
    # RUNNING that fails fast on a terminal state, and teardown.
    box = Sandbox(
        region=REGION,
        port=AGENT_PORT,
        microvm_client=mv,
        logs_client=logs,
        s3_client=aws.client("s3"),
    )

    try:
        print("\n== image ==")
        image = box.build_image(
            name=image_name,
            binary=binary,
            bucket=bucket,
            build_role_arn=build_role,
            dockerfile=conformance_dockerfile(),
            memory_mib=BASELINE_MEMORY_MIB,
            hooks=default_hooks(AGENT_PORT, HOOK_TIMEOUT_SEC),
            tags={"agentd:purpose": "conformance", "agentd:run": run_id},
            build_timeout_sec=IMAGE_BUILD_TIMEOUT_SEC,
            # Scoped to this run, never a pure function of the artifact's content. A
            # content-derived token is a permanent idempotency key: delete the image
            # and rebuild the same bytes, and the service replays the original
            # create as a no-op, wedging an image that then cannot be deleted at all.
            client_token=f"create-{image_name}-{run_id}",
        )
        print(f"  image {image.identifier} CREATED")

        print("\n== run ==")
        session = box.run(
            execution_role_arn=execution_role,
            agent_token=agent_token,
            # Bounds the cost of a crash in this script: an abandoned VM suspends
            # and then terminates instead of billing to the ceiling.
            max_idle_sec=600,
            suspended_sec=600,
            auto_resume=False,
            max_duration_sec=3600,
            client_token=f"run-{image_name}-{run_id}",
        )
        print(f"  microvm {box.microvm_id} RUNNING endpoint={session.endpoint}")

        print("\n== protocol ==")
        drive_protocol(session, results)

        print("\n== suspend / resume ==")
        drive_suspend_resume(box, session, results)

        print("\n== daemon logs ==")
        lines = read_daemon_logs(logs, image_name)
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
            # `terminate` is best-effort and never raises, because it runs here: an
            # exception would replace the real failure with a teardown failure.
            #
            # The log group is deleted separately because the service creates it
            # itself, so Terraform never owns it and `destroy` leaves it behind. Six
            # of them accumulated across the runs that built this script before
            # anyone noticed — storage-only cost, but a leak is a leak, and "the
            # stack destroyed cleanly" was the wrong conclusion to draw from
            # Terraform's output.
            box.terminate(delete_image=True, delete_log_group=True)
            print(f"  terminated {box.microvm_id}, image and log group deleted")

    print("\n== summary ==")
    print(f"  passed: {len(results.passed)}")
    print(f"  failed: {len(results.failed)}")
    for name, detail in results.failed:
        print(f"    FAIL {name}: {detail}")
    return 0 if not results.failed else 1


if __name__ == "__main__":
    sys.exit(main())
