#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["boto3>=1.40", "httpx>=0.27"]
# ///
# SPDX-License-Identifier: Apache-2.0
"""Live conformance run driving the **Rust** client stack through the `microvm` CLI.

This is the only live suite, and it now expresses **every named check** — 85 of them, with
none recorded SKIP. `conformance/run.py` was the oracle — 56 checks through the Python
client — and it went away with that client once both suites ran green against real AWS on
the same commit (Python 56/56, this one 38/38 with 34 recorded SKIP). Those 34 were the
protocol-detail
half only that client could reach: file transfer, tar round trips, the four hostile
archives, SSE ordering, the stdin lifecycle, double-ack, the 8 MiB cap trio, the
identity-repair health flags. `docs/CLI-COVERAGE-PLAN.md` grew the five CLI doors that
close them — `health`, exec identity (`--exec-id`/`--poll`), `ack`, `cp`, `--stream`,
`stdin` — and this file is the flip: every `UNSUPPORTED` entry became a real check body,
with the **name byte-identical** to the one run.py gave it. That is deliberate, and it is
the whole reason the names were preserved through a release where they meant nothing:
this report diffs line for line against the last recorded oracle run in git history, and
each of those 34 lines reads `SKIP` there and `PASS` here.

75 rather than 72, and every delta is worth naming rather than rounding away.
`docs/CLI-COVERAGE-PLAN.md` counted 72 by adding the 38 this suite expressed to the 34 it
did not; two of those 38 — `health reachable through the endpoint` and `platform ran the
run hook before forwarding traffic` — were *weak* readings asserted off the launch envelope
because no `microvm health` existed, and they are now asserted directly against the health
envelope in `drive_health`. The launch keeps one new check of its own in their place
(`the launch reported an endpoint to attach to`). `drive_exec_identity` adds two:
`polling reads an exec without consuming it` — the read-only property, which the oracle
never asserted because its `poll()` could not be confused with an ack — and
`a detached start reports running rather than a verdict`.

The first live round subtracted one. `tree packed in the guest` ran `tar cf` inside the VM
and was deleted rather than fixed: al2023-minimal ships no `tar` binary, and a step needing
one tests the base image's tooling rather than this client. The daemon packs and extracts
through its own `/v1/fs/tar` routes, which is what `cp --tar` drives now.

77 rather than 75: the count had drifted to 76 before anyone updated this line (a measured
live run reported "76 of 76" against three places claiming 75), and the build log-group
delete described in `drive_teardown` is the 77th. The summary block at the end derives the
figure from the checks that ran, so read the number off a run rather than from here.

85 rather than 77, and the eight are the two long-run contracts
`docs/HARNESS-CAPABILITIES.md` said worked by design with nothing testing them. Gap 5 —
detached exec outliving the 60-minute proxy-token ceiling — is `drive_token_rotation`,
four checks against the suite's own VM. Gap 6's unmeasured tail — "a poll from outside
should reset the idle timer" — is `drive_idle_keepalive`, four checks (three
measurements plus its own teardown, counted for the same reason the log-group delete
is) against a second VM launched with the model's minimum idle window and deliberately
run to the edge of it, twice. That section is the slow one and its own output says how
long it takes.

A hybrid driver, and both lanes are deliberate
----------------------------------------------

1. **The CLI, through `--json` envelopes.** The client under test, and now the whole
   protocol surface: lifecycle, exec identity, file and tar transfer, streaming, stdin,
   health. Every invocation also verifies CLI-4 for free — `Cli.call` parses the whole of
   stdout as one JSON document, so a stray `println!` anywhere in the Rust crate turns
   this suite red rather than being noticed by nobody.

   `exec --stream` is the one documented exception and has its own reader,
   `Cli.call_stream`, which asserts the shape rather than tolerating it: every line but
   the last parses as an event, the last parses as the envelope, and its `type` is
   `microvm.exec.stream` rather than `microvm.exec`. A streaming invocation read with
   `Cli.call` would fail on the parse, which is correct — the two shapes are different
   contracts and the driver should not have one function that accepts either.

2. **Raw `httpx`, for six checks that test the DAEMON.** The raw run-hook POST and the
   raw status-code sends. These are the two reach-arounds `conformance/run.py` documented
   before it was deleted: the only callers of `/run` are the platform itself and an
   attacker inside the VM, and the other four assert on a status integer the daemon chose.
   They are not about the client, so the client they go through does not matter — and
   adding a raw-request escape to the CLI so they could go through it would violate CLI-2
   and CLI-5 to make a report look tidier.

   Raw rather than through a client library, and that is the *stronger* shape for what
   these six mean. They assert on the status integer directly — 409, 200, 401, 400 —
   where the deleted Python suite asserted on the exception its own taxonomy mapped that
   integer to. One layer fewer between the daemon's decision and the assertion about it,
   and no way for a client's status table to be the thing that passes.

The third lane is gone. `unsupported()` and the `UNSUPPORTED` table were the honest
record of a real gap; with the gap closed, keeping them would be the opposite of honest.
`Results.skipped` stays as a list and stays in the summary, printed as a count that
should read zero — because a suite that removed its own ability to report a skip is a
suite whose next gap is silent.

What is asserted through the CLI rather than around it
------------------------------------------------------

Every hostile archive is now a real live check. The four archives are built here with
`tarfile`, exactly as run.py built them (GNU tar sanitizes several of these, which is why
they are hand-built), written to a temp file, and handed to `microvm cp --tar`. The
expected outcome is the **daemon's** refusal surfacing as `data.kind: ProtocolError` with
exit 5 — not this suite's opinion of the archive, and not the CLI's. The CLI deliberately
does not pre-validate an archive (`microvms-cli/src/commands/attached.rs`, and the
byte-scan guard in `src/guards.rs` that proves it), because a client-side check would make
these four checks pass against the client's copy of the member rules while the extractor
that actually runs in production went untested.

Money
-----

This run creates real MicroVMs and is billable, ~20 min — ~15 for the main flow plus
about five for `drive_idle_keepalive`, which launches a second VM (from the image already
built, so no second build) and deliberately waits out a 60-second idle window twice. It
belongs to `mise run live` and is never hooked. `--self-test` is the offline half: it drives the
envelope-to-exception mapping and the NDJSON stream reader against a stub `microvm`
script and touches no account.

Usage:
    conformance/run_rs.py --self-test          # offline, free
    conformance/run_rs.py --binary target/aarch64-unknown-linux-musl/release/agentd \
        --microvm-binary target/release/microvm
"""

from __future__ import annotations

import argparse
import itertools
import json
import os
import secrets
import shlex
import stat
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import boto3
import httpx

SERVICE = "lambda-microvms"
REGION = os.environ.get("AWS_REGION", "us-east-1")
AGENT_PORT = 9000
BASELINE_MEMORY_MIB = 1024
# Every public ARM64 base we measured (al2023-minimal, python:3.12-slim,
# node:20-slim, 2026-08-05) leaves WorkingDir empty, so a baked WORKDIR is the only
# way to test cwd inheritance at all. The deleted oracle used the same value.
BAKED_WORKDIR = "/opt/baked-workdir"
# Long enough for a frozen guest and a running one to be distinguishable: a live
# ticker adds roughly forty entries across this window. The oracle used the same 40s.
SUSPEND_WINDOW_SEC = 40

#: The managed base image the Rust client defaults to.
DEFAULT_BASE_IMAGE = "al2023-1"

#: Managed base image name -> the Dockerfile `FROM` that pairs with it. A map rather
#: than one loose literal because the two must agree and used to be able to disagree:
#: the *name* goes into `baseImageArn`, the *ref* goes into the Dockerfile `FROM`, and
#: `microvms-core` refuses a Dockerfile whose `FROM` disagrees with the create call's
#: `baseImageArn` (`control/artifact.rs:233`). Pairing them means selecting one thing
#: and having both follow, which is the shape the deleted `sandbox.BASE_IMAGES` had.
#:
#: `al2023-1` is the managed base every measurement in `docs/PLATFORM.md` from
#: 2026-08-06 onward used, paired with the `amazonlinux:2023-minimal` registry ref the
#: same builds used as `FROM`. Literals here rather than a read of
#: `microvm constants --emit-json`, because that dump carries API constraints and not
#: base images — see `resolve_base_ref` for what that costs.
BASE_IMAGE_REFS = {
    "al2023-1": "public.ecr.aws/amazonlinux/amazonlinux:2023-minimal",
}


# ── the envelope, and the exception it becomes ───────────────────────────────


@dataclass(frozen=True)
class Envelope:
    """One `--json` invocation's whole stdout, parsed.

    Every field unconditional on the failure side, which is the CLI's own contract
    (`microvms-cli/src/envelope.rs:24`: "A key that appears conditionally is a key
    every consumer has to guard"). This dataclass takes it at its word and reads them
    directly, so a field that goes missing is a `KeyError` here rather than a `None`
    that flows into an assertion and passes.
    """

    status: str
    api_version: str
    #: The success discriminant (`microvm.run`, `microvm.state`, ...). Empty on failure.
    type: str
    data: dict[str, Any]
    #: `ERR_*`, empty on success.
    code: str = ""
    exit_code: int = 0
    error: str = ""
    finding: str = ""
    suggestions: tuple[str, ...] = ()

    @property
    def kind(self) -> str | None:
        """The daemon's own status name, or `None` when nothing reached the daemon.

        `data.kind` is a `microvms_core::WireKind` — `Conflict`, `NotFound`,
        `ProtocolError`. Absent for a local rejection, and that absence is
        information: it says the CLI refused before any call.
        """
        found = self.data.get("kind")
        return str(found) if found is not None else None

    @classmethod
    def parse(cls, document: dict[str, Any]) -> Envelope:
        status = str(document["status"])
        data = dict(document.get("data") or {})
        if status == "ok":
            return cls(
                status=status,
                api_version=str(document["apiVersion"]),
                type=str(document["type"]),
                data=data,
            )
        return cls(
            status=status,
            api_version=str(document["apiVersion"]),
            type="",
            data=data,
            code=str(document["code"]),
            exit_code=int(document["exitCode"]),
            error=str(document["error"]),
            finding=str(document["finding"]),
            suggestions=tuple(document["suggestions"]),
        )


class KindError(Exception):
    """A failure envelope, as something `Results.raises` can assert on.

    **Why the kind and not the code.** `Results.raises` in the deleted oracle asserted the client
    exception *type* — `Conflict` versus `NotFound` — because "a 404 arriving where a
    400 belongs fails here as loudly as it should". The CLI's exit code cannot carry
    that: `microvms-cli/src/exit.rs:40` collapses five `WireKind`s onto one
    `ERR_PROTOCOL` deliberately, since "a shell branching on `$?` cannot act
    differently on a 400 than on a 409".

    So the code is the wrong granularity for this suite by construction, not by
    accident, and `data.kind` is the field the CLI added for exactly this consumer
    (`envelope.rs:28` names `conformance/run_rs.py` in as many words). This exception
    carries all three — kind, code, exit code — so a check can assert at whichever
    granularity it means, and the summary can print the coarse one beside the fine.
    """

    def __init__(self, envelope: Envelope) -> None:
        super().__init__(f"{envelope.code}: {envelope.error}")
        self.envelope = envelope
        self.kind = envelope.kind
        self.code = envelope.code
        self.exit_code = envelope.exit_code

    def __repr__(self) -> str:
        return (
            f"KindError(kind={self.kind!r}, code={self.code!r}, exit={self.exit_code})"
        )


class EnvelopeError(Exception):
    """Stdout was not exactly one JSON envelope. A CLI-4 violation, not a check failure.

    Its own type so it is never mistaken for a protocol result: a second document on
    stdout means the *binary* is wrong, and reporting that as "the daemon answered
    oddly" would send the reader to the wrong crate.
    """


# ── the driver ──────────────────────────────────────────────────────────────


@dataclass
class Cli:
    """The `microvm` binary, as a callable that returns envelopes.

    Every call passes `--json` and `--quiet`. `--quiet` because progress on stderr is
    noise in a suite this long, and it is safe: `envelope.rs:12` guarantees `--quiet`
    cannot suppress a leak warning, which is the one line here worth reading.
    """

    binary: Path
    #: Prepended to every invocation's argv. Region only — the three infra values go
    #: through the environment, matching how a human runs it.
    region: str = REGION
    #: Every invocation, for the report. A suite that cannot say what it ran is a
    #: suite whose failures cannot be reproduced by hand.
    log: list[str] = field(default_factory=list)

    def argv(self, *args: str) -> list[str]:
        return [str(self.binary), "--json", "--quiet", *args]

    def call(self, *args: str, timeout: float = 900.0) -> Envelope:
        """One invocation. Raises `KindError` on a failure envelope.

        The exit code is cross-checked against the envelope's own `exitCode` rather
        than trusted from either side alone. They are two independent renderings of
        one decision — `exit.rs`'s table and `main`'s exit — and CLI-3 is the claim
        that they agree, so a suite that read only one would not be checking it.
        """
        argv = self.argv(*args)
        self.log.append(shlex.join(argv))
        proc = subprocess.run(
            argv, capture_output=True, text=True, check=False, timeout=timeout
        )
        envelope = self.parse_stdout(proc.stdout, argv)
        if envelope.status == "error":
            if proc.returncode != envelope.exit_code:
                raise EnvelopeError(
                    f"{shlex.join(argv)} exited {proc.returncode} but its envelope says "
                    f"exitCode {envelope.exit_code}. CLI-3 is the claim that those agree."
                )
            raise KindError(envelope)
        # A success envelope with a non-zero exit is legal and is the `already_reported`
        # case — a workload that exited 4, a suspend that reached TERMINATED. Recorded on
        # the envelope's data by the caller rather than raised, because the payload really
        # is the right answer.
        return envelope

    def call_stream(
        self, *args: str, timeout: float = 900.0
    ) -> tuple[list[dict[str, Any]], Envelope]:
        """One `exec --stream` invocation, as (events, final envelope).

        **The one invocation with a different stdout contract**, and this function asserts
        that contract rather than tolerating it. `microvm manifest` publishes it as
        `exec`'s `alternateResponse`: NDJSON, one event object per line, the envelope last,
        with `type: microvm.exec.stream` rather than `microvm.exec`.

        Three things are checked here, and each is a way the shape can be wrong while the
        command still looks like it worked:

        * every line parses as JSON on its own — a partial or multi-line record would
          make a line-reading consumer lose an event;
        * the **last** line is the envelope and the ones before it are not — an envelope
          written first (or pretty-printed, which makes it several lines) would have a
          consumer hit the terminator before any output;
        * the discriminant is the streaming one, so a consumer branching on `type` learns
          which parse applied from the field it reads first.

        A separate function from `call` rather than a flag on it, deliberately: `call`'s
        whole assertion is that stdout is *one* document, and a function that accepted
        either shape would weaken that for the sixty invocations that are not streams.
        """
        argv = self.argv(*args)
        self.log.append(shlex.join(argv))
        proc = subprocess.run(
            argv, capture_output=True, text=True, check=False, timeout=timeout
        )
        lines = [line for line in proc.stdout.splitlines() if line.strip()]
        if not lines:
            raise EnvelopeError(
                f"{shlex.join(argv)} wrote nothing to stdout. A stream emits one event "
                f"per line and the envelope last.\nstderr:\n{proc.stderr[:400]}"
            )

        documents: list[dict[str, Any]] = []
        for index, line in enumerate(lines):
            try:
                parsed = json.loads(line)
            except json.JSONDecodeError as exc:
                raise EnvelopeError(
                    f"{shlex.join(argv)} line {index} is not one JSON document ({exc}). "
                    f"A streamed exec writes NDJSON — one object per line — so a record "
                    f"spanning lines makes a line-reading consumer lose it. Line was:\n"
                    f"{line[:400]}"
                ) from None
            if not isinstance(parsed, dict):
                raise EnvelopeError(
                    f"{shlex.join(argv)} line {index} is a {type(parsed).__name__}"
                )
            documents.append(parsed)

        *events, final = documents
        if "status" not in final:
            raise EnvelopeError(
                f"{shlex.join(argv)}'s last line is not the envelope: {str(final)[:200]}. "
                "The envelope goes last precisely so a consumer reading line by line "
                "receives every event before the terminator."
            )
        for index, event in enumerate(events):
            if "status" in event:
                raise EnvelopeError(
                    f"{shlex.join(argv)} line {index} looks like an envelope rather than "
                    f"an event. Exactly one envelope per invocation, and it is the last "
                    f"line: {str(event)[:200]}"
                )
        envelope = Envelope.parse(final)
        if envelope.status == "error":
            if proc.returncode != envelope.exit_code:
                raise EnvelopeError(
                    f"{shlex.join(argv)} exited {proc.returncode} but its envelope says "
                    f"exitCode {envelope.exit_code}. CLI-3 holds on the streaming path too."
                )
            raise KindError(envelope)
        if envelope.type != "microvm.exec.stream":
            raise EnvelopeError(
                f"{shlex.join(argv)} streamed but announced {envelope.type!r}. The "
                "streaming shape must carry its own discriminant, or a consumer "
                "branching on `type` cannot tell which parse to use."
            )
        return events, envelope

    @staticmethod
    def parse_stdout(stdout: str, argv: list[str]) -> Envelope:
        """The whole of stdout as one document. This *is* CLI-4's assertion.

        `json.loads` over the entire stream rather than the first line, so a second
        envelope, a progress line, or a stray `print` all fail here. That is the same
        assertion `tests/exit_codes.rs` makes in-crate; making it again on every one of
        this suite's invocations is cheap and covers the paths a unit test cannot reach.
        """
        try:
            document = json.loads(stdout)
        except json.JSONDecodeError as exc:
            raise EnvelopeError(
                f"{shlex.join(argv)} did not write exactly one JSON document to stdout "
                f"({exc}). Progress belongs on stderr (CLI-4). stdout was:\n{stdout[:400]}"
            ) from None
        if not isinstance(document, dict):
            raise EnvelopeError(f"{shlex.join(argv)} wrote a {type(document).__name__}")
        return Envelope.parse(document)


# ── results ─────────────────────────────────────────────────────────────────


@dataclass
class Results:
    """Every check's outcome, so the summary reports facts rather than a feeling.

    The four primitives are the oracle's, with the same names and the same semantics.
    `skipped` is a *third* list rather than a pass with a note, because a skip folded into
    `passed` is how a suite that covers half of what it claims looks identical to one that
    covers all of it.

    It is now always empty, and it stays here anyway. The `unsupported()` primitive that
    filled it was the honest record of 34 checks this client could not express; those
    surfaces landed and the entries became real check bodies. Deleting the list along with
    them would remove the suite's ability to *say* a gap exists — so the count is still
    printed, and it should read zero. The next gap gets a line rather than a silence.
    """

    passed: list[str] = field(default_factory=list)
    failed: list[tuple[str, str]] = field(default_factory=list)
    skipped: list[tuple[str, str]] = field(default_factory=list)
    #: Set on the throwaway `Results` the self-test's negative twins run against, where a
    #: FAIL is the *expected* outcome. Only changes the printed marker — `PROBE` rather
    #: than `FAIL` — because a suite whose green run prints three FAIL lines is a suite
    #: whose real failures get skimmed past, which is the same reasoning `mise.toml`
    #: gives for keeping a RuntimeWarning out of `live:rates`.
    probe: bool = False

    def check(self, name: str, ok: bool, detail: str = "") -> bool:
        if ok:
            self.passed.append(name)
            print(f"  PASS  {name}" + (f" — {detail}" if detail else ""))
        else:
            self.failed.append((name, detail))
            marker = "PROBE" if self.probe else "FAIL "
            print(f"  {marker} {name} — {detail}")
        return ok

    def eq(self, name: str, actual: Any, expected: Any) -> bool:
        return self.check(
            name, actual == expected, f"expected {expected!r}, got {actual!r}"
        )

    def raises(self, name: str, expected_kind: str, call: Callable[[], Any]) -> bool:
        """Asserts a call fails with exactly the `WireKind` named by `expected_kind`.

        The kind rather than the `ERR_*` code, for the reason `KindError`'s docstring
        gives: five kinds share `ERR_PROTOCOL`, so a code comparison would pass for a
        404 where a 400 belongs — which is the precise confusion the daemon's status
        discipline exists to prevent, and the one this primitive is here to catch.

        `EnvelopeError` is deliberately not caught. A malformed stdout is a defect in
        the binary, and reporting it as "the wrong kind was raised" would name the
        daemon for the CLI's mistake.
        """
        try:
            call()
        except KindError as exc:
            if exc.kind == expected_kind:
                return self.check(
                    name, True, f"{exc.kind} ({exc.code}, exit {exc.exit_code})"
                )
            return self.check(
                name,
                False,
                f"expected kind {expected_kind!r}, got {exc.kind!r} "
                f"({exc.code}, exit {exc.exit_code})",
            )
        return self.check(
            name, False, f"expected kind {expected_kind!r}, nothing raised"
        )

    def ok(self, name: str, call: Callable[[], Any]) -> bool:
        """Asserts a call succeeds, which for this driver means "no failure envelope"."""
        try:
            call()
        except Exception as exc:  # noqa: BLE001 - any error is the finding
            return self.check(name, False, repr(exc))
        return self.check(name, True)

    def skip(self, name: str, reason: str) -> None:
        """Records a check this client has no way to express.

        Printed as `SKIP` and counted apart from both passes and failures. It does not
        fail the run — a suite that is permanently red is a suite people stop reading —
        but it is never silent, which is the whole difference between a coverage statement
        and a gap.

        **No caller, on purpose.** This was `unsupported()` and it had 34; the CLI grew the
        surfaces and every one became a real check. It is kept as the shape the next gap
        takes, because the alternative is that the next gap has nowhere to be recorded and
        gets a comment instead. `--self-test` calls it once against a throwaway `Results`
        so it cannot rot into something that no longer runs.
        """
        self.skipped.append((name, reason))
        print(f"  SKIP  {name} — {reason}")


# ── the four hostile archives ────────────────────────────────────────────────


def build_hostile_archives() -> list[tuple[str, bytes]]:
    """The four malicious archives, hand-built, exactly as the deleted oracle built them.

    `tarfile` rather than `tar(1)`, and that is not a convenience: GNU tar **sanitizes**
    several of these — it strips a leading `../`, refuses to store an absolute link target
    — so shelling out would produce four harmless archives and four checks that passed
    against nothing. Each `TarInfo` is constructed field by field here so the hostile
    member really is in the bytes.

    Every one of these is a refused *member*, which the daemon answers 400 for and the
    client maps to `ProtocolError`. A 413 would be a cap violation instead, and the
    distinction matters: one means "this archive is hostile", the other means "this archive
    is merely too big".

    The names are the oracle's, so the four `hostile archive refused: <name>` lines in this
    report diff against the four `SKIP` lines in the last one.
    """
    import io
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
        # Writes outside the extraction root by walking up out of it.
        ("parent traversal", make([("../../escaped.txt", "file", None, b"pwned")])),
        # A symlink pointing at an absolute path in the guest, which would let a later
        # write land on /etc/passwd.
        ("absolute link target", make([("link", "sym", "/etc/passwd", b"")])),
        # The two-member version, which defeats a naive per-member path check: `s` is an
        # in-tree symlink to `..`, and `s/escaped.txt` then resolves outside the root
        # without any member's own name containing `..`.
        (
            "symlink redirect",
            make([("s", "sym", "..", b""), ("s/escaped.txt", "file", None, b"pwned")]),
        ),
        # A character device. Extracting one means the archive can create a node that
        # reads host memory or produces attacker-chosen bytes.
        ("character device", make([("dev", "dev", None, b"")])),
    ]


# ── the daemon-level checks, on raw httpx ────────────────────────────────────


@dataclass
class Daemon:
    """Raw HTTP to one MicroVM's daemon, through the platform's endpoint proxy.

    Not a client library and deliberately so: every method here returns the status
    integer the daemon chose, and the six checks that use it assert on that integer.
    The deleted Python suite asserted on the exception *its* status table mapped the
    integer to, which is one more layer that could be the thing that passes. Status
    codes are what those checks always meant.

    **Two headers, not one.** `X-aws-proxy-auth` carries a JWE scoped to a MicroVM id
    and a port set; `X-aws-proxy-port` names which of that token's allowed ports this
    request targets. Omitting the second is a rejection that reads like a bad token.
    Both measured 2026-08-05; see `docs/PLATFORM.md`.
    """

    endpoint: str
    agent_token: str
    microvm_id: str
    #: The boto3 `lambda-microvms` client, for minting the proxy token.
    microvm_client: Any
    port: int = AGENT_PORT
    timeout: float = 60.0
    _client: httpx.Client | None = None
    _proxy_token: str | None = None

    def __post_init__(self) -> None:
        if not self.endpoint.startswith("http"):
            self.endpoint = f"https://{self.endpoint}"
        self._client = httpx.Client(timeout=self.timeout, verify=True)

    def close(self) -> None:
        if self._client is not None:
            self._client.close()

    def proxy_token(self) -> str:
        """The endpoint proxy token, minted once and cached for this run.

        `authToken` is a **map of header name to value**, not a bare string — the API
        is shaped for schemes needing more than one header, and reading it as a string
        is one of the six defects the first live run found. 60 minutes is the ceiling
        the service enforces rather than a choice, and it comfortably outlasts the six
        checks below, so there is no refresh path here (the CLI's own client has one).
        """
        if self._proxy_token is None:
            response = self.microvm_client.create_microvm_auth_token(
                microvmIdentifier=self.microvm_id,
                expirationInMinutes=60,
                allowedPorts=[{"port": self.port}],
            )
            self._proxy_token = str(response["authToken"]["X-aws-proxy-auth"])
        return self._proxy_token

    def headers(self, token: str | None) -> dict[str, Any]:
        """Both proxy headers, plus the bearer when one was named.

        `token=None` means send no `Authorization` at all, which is how the hook route
        is exercised — so it is a real value here rather than "use the default".

        The bearer is **bytes**, not str. httpx encodes a str header as ASCII and
        refuses anything else, which would make the non-ASCII token check below
        unsendable; the daemon's stated property is that it compares header bytes
        without decoding them, and that is only testable if a client can put arbitrary
        bytes on the wire. Verified: `httpx.Headers({"Authorization": "Bearer tökén"})`
        raises `UnicodeEncodeError`, and the bytes form puts
        `b'Bearer t\\xc3\\xb6k\\xc3\\xa9n'` on the wire.
        """
        headers: dict[str, Any] = {
            "X-aws-proxy-auth": self.proxy_token(),
            "X-aws-proxy-port": str(self.port),
        }
        if token is not None:
            headers["Authorization"] = b"Bearer " + token.encode("utf-8")
        return headers

    def status(
        self,
        method: str,
        path: str,
        *,
        token: str | None = None,
        json_body: Any = None,
    ) -> int:
        """One request, and the status the daemon answered with.

        Never raises on a status: these checks assert on 401 and 409 as *expected*
        outcomes, and a caller that could only reach them through exceptions would be
        a caller that cannot test the protocol. A wire failure still raises, because a
        dropped connection is not a status and must not be reported as one — which is
        exactly what the non-ASCII token check is asserting the daemon does not do.
        """
        assert self._client is not None
        response = self._client.request(
            method,
            f"{self.endpoint}{path}",
            headers=self.headers(token),
            json=json_body,
            timeout=self.timeout,
        )
        return response.status_code


def post_run_hook(daemon: Daemon, token: str) -> int:
    """Posts the platform's run hook and returns the raw status.

    Reached around the client under test for the reason the deleted oracle gave: the
    only callers of this route are the platform itself and an attacker inside the VM,
    so an affordance for it in *any* client would be a footgun with no legitimate use.
    Adding one to the CLI to make this suite tidier would break CLI-2 and CLI-5 both.

    No `Authorization` header, and the body is the platform's envelope rather than our
    payload directly: the string given to `RunMicrovm` arrives wrapped as
    `{"runHookPayload": "<it>"}`.
    """
    return daemon.status(
        "POST",
        "/aws/lambda-microvms/runtime/v1/run",
        token=None,
        json_body={"runHookPayload": json.dumps({"agent_token": token})},
    )


def drive_daemon_lane(daemon: Daemon, results: Results) -> None:
    """The six checks that test the daemon rather than the client under test.

    Same six names the oracle used, asserting on the status integer the daemon chose.
    Routing them through a client library would test that library twice and the daemon
    no better; asserting on the integer is what they always meant.
    """
    print("\n-- bootstrap and authorization (daemon lane) --")
    results.eq(
        "post-bootstrap hijack refused with 409",
        post_run_hook(daemon, "attacker-token"),
        409,
    )
    results.eq(
        "identical bootstrap replay accepted",
        post_run_hook(daemon, daemon.agent_token),
        200,
    )

    for name, token in (
        ("wrong token refused with 401", "wrong-token"),
        # The daemon must *answer* a token it cannot decode rather than drop the
        # connection, which is why this asserts a status at all: a `TransportError`
        # out of `Daemon.status` is the failure, and `results.eq` reports it as the
        # exception it is rather than as a wrong status.
        ("non-ASCII token header answered, not a dropped connection", "tökén"),
    ):
        results.eq(name, daemon.status("GET", "/v1/exec/nope", token=token), 401)

    for name, method, path, body in (
        ("malformed body is 400, not 404", "POST", "/v1/exec/start", {"bogus": True}),
        ("missing path key is 400", "GET", "/v1/fs/file", None),
    ):
        # 400 rather than 404 is the whole assertion, and it is why this compares the
        # integer instead of catching a class: the deleted client mapped both onto
        # separate exceptions, but the defect being guarded against — a phantom
        # missing file where the request was simply malformed — is a 404 arriving
        # where a 400 belongs, and only the integer says which came back.
        results.eq(
            name,
            daemon.status(method, path, token=daemon.agent_token, json_body=body),
            400,
        )


# ── the CLI lane ────────────────────────────────────────────────────────────


def conformance_dockerfile(base_ref: str) -> str:
    """The image recipe, with a WORKDIR baked in so cwd inheritance is testable.

    The `FROM` is taken from the CLI's own manifest rather than written here, because
    `microvms-core` refuses a Dockerfile whose `FROM` disagrees with the `baseImageArn`
    the create call sends (`control/artifact.rs:233`) — and that refusal is correct, so
    hardcoding a ref here would make this suite fail on a base-image change with a
    message about a Dockerfile rather than about a base image.
    """
    return "\n".join(
        [
            f"FROM {base_ref}",
            "COPY agentd /agentd",
            "RUN chmod 0755 /agentd",
            f"RUN mkdir -p {BAKED_WORKDIR}",
            f"WORKDIR {BAKED_WORKDIR}",
            f"ENV AGENTD_PORT={AGENT_PORT}",
            "ENV AGENTD_LOG=info",
            f"EXPOSE {AGENT_PORT}",
            "ENTRYPOINT []",
            'CMD ["/agentd"]',
            "",
        ]
    )


def resolve_base_ref() -> str:
    """The Dockerfile `FROM` the Rust client pairs with its default base image.

    A module table (`BASE_IMAGE_REFS`) rather than a value read out of the client under
    test, which is a real limitation and worth naming. It used to be read from the
    Python client's `BASE_IMAGES`, held equal to the Rust one by `check-model-drift`'s
    cross-comparison; that table went with the client. Reading it from
    `microvm constants --emit-json` would be better and is not possible — that dump
    carries API constraints, not base images.

    The failure mode is bounded and loud. If `microvms-core`'s default base image
    changes and this table does not, the build fails on `control/artifact.rs:233`'s
    refusal — the `FROM` disagreeing with `baseImageArn` — which names both values.
    That is a suite that fails to run rather than one that passes wrongly, and it is
    the same shape the check already had when the two tables could disagree. A base
    image not in the table fails here instead, before anything is launched.
    """
    try:
        return BASE_IMAGE_REFS[DEFAULT_BASE_IMAGE]
    except KeyError:
        raise SystemExit(
            f"no Dockerfile FROM paired with base image {DEFAULT_BASE_IMAGE!r}. The name "
            "alone does not say what `FROM` goes with it, and guessing is how the two fell "
            f"out of step before — add the pair to BASE_IMAGE_REFS (have: "
            f"{', '.join(sorted(BASE_IMAGE_REFS))})."
        ) from None


def drive_local_commands(cli: Cli, results: Results) -> None:
    """The commands that reach no account. Free, and they check the CLI's own contract.

    Run first and deliberately: every one of them is a CLI-4 assertion (`Cli.call`
    parses stdout whole), and finding a stray `println!` before spending fifteen
    minutes on a build is worth the two seconds.
    """
    print("\n-- local commands (no account) --")
    results.ok("ls reports the local ledger", lambda: cli.call("ls"))
    results.ok(
        "cost reports a labelled estimate",
        lambda: cli.call("cost", "--running-sec", "3600"),
    )

    manifest = cli.call("manifest")
    commands = [entry["name"] for entry in manifest.data["commands"]]
    results.check(
        "manifest lists every command this suite drives",
        {"run", "exec", "suspend", "resume", "terminate"} <= set(commands),
        f"{len(commands)} commands",
    )

    # `logs` FAILS by design and the distinction is load-bearing: an empty `lines`
    # array is the wire shape for "the group exists and has no events", which is
    # exactly what a wrong build-role prefix produces. Asserting the refusal is
    # asserting that the two stay distinguishable.
    try:
        cli.call("logs", "agentd-conformance")
    except KindError as exc:
        results.check(
            "logs names the group and refuses to imply it is empty",
            exc.code == "ERR_PRECONDITION" and exc.envelope.data.get("lines") is None,
            f"{exc.code}, lines={exc.envelope.data.get('lines')!r}",
        )
    else:
        results.check(
            "logs names the group and refuses to imply it is empty",
            False,
            "logs succeeded, which would mean it read CloudWatch",
        )


def drive_lifecycle(
    cli: Cli, binary: Path, dockerfile: Path, results: Results
) -> Envelope:
    """`run --keep`, which is build plus launch plus exec in one invocation.

    `--keep` because every check after this one needs the VM, and the teardown is the
    caller's `finally` — the same shape the oracle used, for the same reason: a teardown
    that runs from inside the happy path is a teardown that does not run when it matters.
    """
    print("\n== run (build + launch + exec) ==")
    launched = cli.call(
        "run",
        str(binary),
        "--name",
        f"microvm-cli-conformance-{secrets.token_hex(4)}",
        "--dockerfile",
        str(dockerfile),
        "--memory",
        str(BASELINE_MEMORY_MIB),
        "--repair-identity",
        "--keep",
        "--region",
        cli.region,
        "--exec",
        "echo live; pwd; id -u",
        "--max-idle-sec",
        "600",
        "--suspended-sec",
        "600",
        "--max-duration-sec",
        "3600",
        timeout=50 * 60,
    )

    results.eq("run emitted its namespaced envelope type", launched.type, "microvm.run")
    # `health reachable through the endpoint` and `platform ran the run hook before
    # forwarding traffic` were asserted here, weakly: a launch that returned at all implied
    # `wait_until_ready` had seen a bootstrapped daemon, which was the strongest reading
    # available without a `microvm health`. Both are now asserted directly in
    # `drive_health`, against the health envelope's own `version` and `bootstrapped`, which
    # is the oracle's original form. Duplicating them here would print each name twice and
    # make the report's own totals a lie — so the launch keeps only what only it can say.
    results.check(
        "the launch reported an endpoint to attach to",
        bool(launched.data.get("endpoint")),
        f"endpoint {launched.data.get('endpoint')!r}",
    )
    results.eq("exec exited 0", launched.data.get("execExitCode"), 0)
    stdout = launched.data.get("stdout") or ""
    results.check("exec start accepted", bool(stdout), repr(stdout[:80]))
    results.check("exec captured stdout", "live" in stdout, repr(stdout[:80]))
    results.check(
        "omitted cwd inherits the image WORKDIR",
        BAKED_WORKDIR in stdout,
        repr(stdout[:120]),
    )
    # `--keep` must say what the caller has taken responsibility for. A kept VM whose
    # identifiers were not reported is a bill with no id attached to it.
    results.check(
        "keep reported the identifiers the caller now owns",
        bool(launched.data.get("microvmId"))
        and bool(launched.data.get("imageIdentifier")),
        f"microvm={launched.data.get('microvmId')} image={launched.data.get('imageIdentifier')}",
    )
    # Cost travels on the envelope whichever way the run ended, and every dollar is
    # labelled an estimate. A `$0.00` where a rate is unpublished is the failure COST-3
    # exists to prevent, so its absence is worth asserting on a real report.
    results.check(
        "the run envelope carries a labelled cost report",
        isinstance(launched.data.get("cost"), dict),
        f"{type(launched.data.get('cost')).__name__}",
    )
    return launched


def attach_args(cli: Cli, launched: Envelope, endpoint: str | None = None) -> list[str]:
    """The identifier triple every attached command takes, plus the region.

    One helper rather than the same six-element list written out in each section, which is
    the same argument `microvms-cli`'s own `AttachFlags` makes: three of the four are
    opaque strings of the same shape, so writing them out repeatedly is repeated chances to
    put an endpoint where a token belongs.

    `endpoint` overrides the launch's, for the post-resume sections: `resume` hands back the
    endpoint it read, and following that rather than the launch's is what makes a changed
    endpoint a followed change instead of a silent failure.
    """
    return [
        "--endpoint",
        endpoint or str(launched.data["endpoint"]),
        "--agent-token",
        str(launched.data["agentToken"]),
        "--microvm-id",
        str(launched.data["microvmId"]),
        "--region",
        cli.region,
    ]


def drive_exec(cli: Cli, launched: Envelope, results: Results) -> None:
    """`microvm exec` against the kept VM — the attach path, which `run` never exercises.

    Worth its own section: `exec` goes through `attach_session` rather than
    `open_sandbox`, so it is the only command that mints a proxy token for a VM this
    process did not launch. TRAP-9 lives on that path.
    """
    print("\n-- exec (the attach path) --")
    attach = attach_args(cli, launched)

    first = cli.call("exec", "echo attached", *attach)
    results.eq("exec exited 0 on the attach path", first.data.get("exitCode"), 0)
    results.check(
        "attached exec captured stdout",
        "attached" in (first.data.get("stdout") or ""),
        repr(first.data.get("stdout")),
    )

    # The oracle's three shell edge cases, same names. `shell: true` with a single script
    # string is what both clients send, so an unbalanced brace must stay one command.
    for name, script in (
        ("empty", ""),
        ("comment-only", "# nothing"),
        ("unbalanced brace", "echo A } echo B"),
    ):
        got = cli.call("exec", script, *attach)
        results.eq(f"{name} shell command exits 0", got.data.get("exitCode"), 0)
        if name == "unbalanced brace":
            results.check(
                "unbalanced brace did not escape into a second command",
                (got.data.get("stdout") or "").strip() == "A } echo B",
                repr(got.data.get("stdout")),
            )

    # A failing workload keeps its *success* envelope and earns ERR_EXEC_FAILED's code.
    # The distinction a CI caller needs: "your tests failed" is not "we never got a VM",
    # and one shared exit code cannot say both.
    proc = subprocess.run(
        cli.argv("exec", "exit 4", *attach), capture_output=True, text=True, check=False
    )
    envelope = Cli.parse_stdout(proc.stdout, cli.argv("exec", "exit 4"))
    results.check(
        "a failing workload reports success with a distinct exit code",
        envelope.status == "ok"
        and envelope.data.get("exitCode") == 4
        and proc.returncode == 13,
        f"status={envelope.status} exitCode={envelope.data.get('exitCode')} $?={proc.returncode}",
    )


def drive_health(cli: Cli, launched: Envelope, results: Results) -> None:
    """`microvm health` — five checks, and the identity pair is the one with a measurement.

    `identity_degraded` is the only guard whose unit tests inject a fake layout, so this is
    the one place the real bind mount over real procfs is exercised. Measured 2026-08-06:
    without `additionalOsCapabilities: ["ALL"]` the hostname and boot_id steps fail with
    EPERM even though the daemon is root, and `identityDegraded` is how that surfaces.
    Asserting it here is what makes the capability requirement impossible to drop by
    accident — the launch above passes `--repair-identity`, and if core stopped injecting
    `["ALL"]` this check would be the thing that noticed.
    """
    print("\n-- health --")
    attach = attach_args(cli, launched)
    health = cli.call("health", *attach)

    results.check(
        "health reachable through the endpoint",
        bool(health.data.get("version")),
        f"daemon version {health.data.get('version')!r}",
    )
    results.eq(
        "platform ran the run hook before forwarding traffic",
        health.data.get("bootstrapped"),
        True,
    )
    results.eq(
        "identity repair completed every step",
        health.data.get("identityDegraded"),
        False,
    )
    results.eq(
        "identity repair actually ran", health.data.get("identityRepaired"), True
    )


def drive_exec_identity(cli: Cli, launched: Envelope, results: Results) -> None:
    """Exec identity: `--exec-id`, `--detach`, `--poll`, and `microvm ack`. Seven checks.

    The idempotency-key property is the interesting one and it needs a stable id to test at
    all — which is why the oracle could express it and the CLI could not until `--exec-id`
    landed. `MUST_NOT_RUN` in the retried command is the falsification: if the daemon
    spawned a second child the string would appear in the output, and the check is a
    substring search for its *absence*.

    **`--detach` is what makes the rest of this section possible**, and the first live round
    is what proved it. `microvm exec` without it is start-wait-**ack**: the ack releases the
    output, so a later explicit `microvm ack` correctly 409s (`already_acked`) and a poll
    correctly reports `acked` with nothing. Two checks here failed exactly that way. This
    section needs an exec whose lifecycle it owns — start, poll while the output is still
    buffered, ack once, watch the second ack refuse — which is the oracle's own
    start/poll/ack decomposition and is now `--detach`'s reason to exist.
    """
    print("\n-- exec identity (--exec-id, --detach, --poll, ack) --")
    attach = attach_args(cli, launched)

    # Detached: started and nothing else. Without `--detach` this invocation would ack its
    # own output and every check below would be reading an already-collected exec.
    started = cli.call(
        "exec", "echo identity-live", "--exec-id", "c1", "--detach", *attach
    )
    results.eq(
        "exec start accepted with a caller-supplied id",
        started.data.get("execId"),
        "c1",
    )
    results.eq(
        "a detached start reports running rather than a verdict",
        started.data.get("phase"),
        "running",
    )

    # The retry: the identical id, a *different* command. The daemon answers success for a
    # known id without spawning anything (`agentd/src/exec.rs:366`, decided under the
    # registry lock), so this must succeed and must not run the new command. Detached again,
    # so the retry does not ack either.
    results.ok(
        "retried start accepted",
        lambda: cli.call(
            "exec", "echo MUST_NOT_RUN", "--exec-id", "c1", "--detach", *attach
        ),
    )

    # `echo` is quick but not instant, and a poll issued in the same breath as the start can
    # legitimately catch `running` with no output yet. Polled until it exits — which is also
    # a live demonstration that polling is repeatable, since that is the property it rests on.
    after = None
    for _ in range(12):
        after = cli.call("exec", "--poll", "c1", *attach)
        if after.data.get("phase") != "running":
            break
        time.sleep(1)
    assert after is not None
    results.check(
        "retried start did not spawn a second child",
        "MUST_NOT_RUN" not in (after.data.get("stdout") or ""),
        repr(after.data.get("stdout")),
    )

    # `--poll` is read-only, so the first exec's own output is still there — which is also
    # what makes the ack below meaningful rather than a no-op. This is the check that caught
    # the missing `--detach`: it read `''` because `exec` had already acked.
    results.check(
        "polling reads an exec without consuming it",
        "identity-live" in (after.data.get("stdout") or ""),
        repr((after.data.get("stdout") or "")[:80]),
    )

    results.ok("ack accepted", lambda: cli.call("ack", "c1", *attach))
    # The second ack is a 409 rather than a 200 with an empty body, because an empty body
    # would read as "the command produced no output" (`agentd/src/exec.rs:854`).
    results.raises(
        "second ack refused with 409",
        "Conflict",
        lambda: cli.call("ack", "c1", *attach),
    )
    results.raises(
        "unknown exec id is 404",
        "NotFound",
        lambda: cli.call("exec", "--poll", "never-existed", *attach),
    )


def drive_output_cap(cli: Cli, launched: Envelope, results: Results) -> None:
    """The 8 MiB cap trio. 32 MiB of output against it.

    The daemon must truncate and **stay up**, not grow until the guest's OOM killer takes
    it. `health` after the fact is the survival probe and is the reason this trio needed
    `microvm health` to be expressible: an exec that answered would also prove the daemon
    lived, but only for a daemon that was still serving *that* route — health is the
    unauthenticated liveness question asked directly.
    """
    print("\n-- large output (the 8 MiB cap) --")
    attach = attach_args(cli, launched)
    noisy = cli.call(
        "exec",
        "dd if=/dev/zero bs=1M count=32 2>/dev/null | tr '\\0' 'x'",
        "--timeout",
        "180",
        *attach,
        timeout=300.0,
    )
    results.eq("noisy command still exits 0", noisy.data.get("exitCode"), 0)
    results.eq("output past the cap was truncated", noisy.data.get("truncated"), True)
    results.ok("daemon survived the truncation", lambda: cli.call("health", *attach))


def drive_file_transfer(
    cli: Cli, launched: Envelope, results: Results, workdir: Path
) -> None:
    """`microvm cp` and `cp --tar`: thirteen checks, including the four hostile archives.

    The symlink pair is the one worth naming: harnesses pack symlinks deliberately, and a
    daemon that refused links would break real uploads — so an in-tree link has to survive
    the round trip *as a link* and still resolve to its target's content. Both halves are
    asserted, because a round trip that dereferenced the link would satisfy the second on
    its own.

    `--tar` is asymmetric, and the asymmetry is the design rather than a rough edge. The
    **local** side is an archive file, because neither `microvms-core` nor the CLI carries a
    tar library — `session/files.rs:112` declines to add one, since Rust's standard library
    has no equivalent of tarfile's `data` filter and "an extraction that looked safe and was
    not is worse than none". The **`vm:`** side is a *directory*, because the daemon does
    carry the crate and both routes are about trees: `GET /v1/fs/tar` packs a directory and
    `PUT /v1/fs/tar` extracts into one, through the confined extractor that stays the only
    extractor in the system.

    So nothing outside the daemon ever packs or unpacks, which is also why this section no
    longer shells out to `tar` in the guest: al2023-minimal has no `tar` binary, and a step
    that needed one would be testing the base image's tooling rather than this client.
    """
    print("\n-- file transfer --")
    attach = attach_args(cli, launched)

    payload = workdir / "live.txt"
    payload.write_bytes(b"written through the endpoint")
    results.ok(
        "single file write accepted",
        lambda: cli.call(
            "cp", str(payload), "vm:/tmp/live.txt", "--mode", "644", *attach
        ),
    )

    read_back = workdir / "read-back.txt"
    cli.call("cp", "vm:/tmp/live.txt", str(read_back), *attach)
    results.eq(
        "single file read returns the bytes",
        read_back.read_bytes(),
        b"written through the endpoint",
    )
    results.raises(
        "read of an absent file is 404",
        "NotFound",
        lambda: cli.call("cp", "vm:/tmp/absent", str(workdir / "absent.txt"), *attach),
    )

    # The tree, built in the guest. A symlink packed deliberately, because that is the
    # member a harness really sends.
    tree = cli.call(
        "exec",
        "rm -rf /tmp/tree /tmp/dest && mkdir -p /tmp/tree/sub && "
        "echo payload > /tmp/tree/a.txt && ln -sf a.txt /tmp/tree/link && "
        "echo deep > /tmp/tree/sub/b.txt",
        *attach,
    )
    results.eq("tree created for the round trip", tree.data.get("exitCode"), 0)

    # `vm:` names the DIRECTORY, and the daemon packs it. That is the whole shape of these
    # two routes and the first live round is what taught it: `GET /v1/fs/tar` requires a
    # directory (`agentd/src/fs.rs:786` — a non-directory is an explicit 400 "use
    # /v1/fs/file") and packs it itself with `pack_tree`, which carries the `tar` crate so
    # that no client and no base image needs one.
    #
    # The first draft of this section ran `tar cf` in the guest and then pointed `--tar` at
    # the resulting file. It failed twice over: al2023-minimal ships no `tar` binary (exit
    # 127), and the file was the wrong thing to hand the route anyway. Both errors came from
    # the same wrong belief — that something other than the daemon had to do the packing.
    # The guest-tar step is gone rather than fixed: it tested the base image's tooling, not
    # this client.
    #
    # Members are `./`-relative (`append_dir_all(".", root)`), so they land *flattened*
    # under the destination — `/tmp/dest/link`, not `/tmp/dest/tree/link`. That is what the
    # verification below reads, and it is what makes a downloaded archive re-uploadable,
    # which `fs.rs:226` names as the one round trip a harness performs constantly.
    archive = workdir / "tree.tar"
    try:
        cli.call("cp", "vm:/tmp/tree", str(archive), "--tar", *attach)
        results.check(
            "tar download succeeded",
            archive.exists() and archive.stat().st_size > 0,
            f"{archive.stat().st_size if archive.exists() else 0} bytes",
        )
    except (KindError, EnvelopeError) as exc:
        results.check("tar download succeeded", False, repr(exc))

    if archive.exists() and archive.stat().st_size > 0:
        results.ok(
            "tar upload accepted",
            lambda: cli.call("cp", str(archive), "vm:/tmp/dest", "--tar", *attach),
        )
    else:
        results.check(
            "tar upload accepted", False, "no archive was downloaded to upload"
        )

    # `readlink` first, so a dereferenced round trip fails on the *link* assertion rather
    # than passing the content one and looking fine. Paths are flattened under the
    # destination, per the note above.
    verify = cli.call(
        "exec",
        "readlink /tmp/dest/link; cat /tmp/dest/link; cat /tmp/dest/sub/b.txt",
        *attach,
    )
    verified = verify.data.get("stdout") or ""
    results.check(
        "symlink survived the round trip as a symlink",
        verified.startswith("a.txt"),
        repr(verified[:120]),
    )
    results.check(
        "symlink still resolves to its target's content",
        "payload" in verified,
        repr(verified[:120]),
    )

    # -- the four hostile archives -------------------------------------------
    #
    # Handed to `microvm cp --tar` as pre-built files. The expected failure is the
    # DAEMON's, surfacing as `data.kind: ProtocolError` with exit 5 — the CLI does not
    # pre-validate an archive, and `microvms-cli/src/guards.rs`'s byte-scan proves it. A
    # client-side check would make these four pass against the client's copy of the member
    # rules while the extractor that runs in production went untested.
    print("\n-- hostile archives --")
    for name, archive_bytes in build_hostile_archives():
        path = workdir / f"hostile-{name.replace(' ', '-')}.tar"
        path.write_bytes(archive_bytes)
        results.raises(
            f"hostile archive refused: {name}",
            "ProtocolError",
            lambda p=path: cli.call("cp", str(p), "vm:/tmp/hostile", "--tar", *attach),
        )

    escaped = cli.call(
        "exec",
        "ls /escaped.txt /tmp/escaped.txt 2>&1 | head -3; echo done",
        *attach,
    )
    listing = escaped.data.get("stdout") or ""
    results.check(
        "nothing escaped the extraction root",
        "No such file" in listing or "cannot access" in listing,
        repr(listing[:160]),
    )


def drive_streaming(cli: Cli, launched: Envelope, results: Results) -> None:
    """`exec --stream`: five checks, and the question is about AWS rather than the daemon.

    Streaming is the capability an agent harness needs and the one no local tier can fully
    validate: whether AWS's endpoint proxy actually **forwards** Server-Sent Events rather
    than buffering them until the command ends. Documentation says it does; this is the
    check, and it is the reason this section is worth its cost.

    `--exec-id` is what makes the last check possible: streaming must not consume the exec,
    so the same id is polled afterwards and its buffered output must still be there.
    """
    print("\n-- streaming --")
    attach = attach_args(cli, launched)

    events, envelope = cli.call_stream(
        "exec",
        "for i in 1 2 3 4 5; do echo chunk-$i; done; echo done-streaming",
        "--stream",
        "--exec-id",
        "stream1",
        *attach,
    )

    outputs = [event for event in events if event.get("event") == "output"]
    gaps = [event for event in events if event.get("event") == "gap"]
    exits = [event for event in events if event.get("event") == "exit"]
    streamed = "".join(str(event.get("text") or "") for event in outputs)

    results.check(
        "SSE reached us through the endpoint proxy",
        bool(outputs),
        f"{len(outputs)} chunk(s), {envelope.data.get('bytes')} bytes",
    )
    results.check(
        "streamed output is complete and ordered",
        "chunk-1" in streamed
        and "chunk-5" in streamed
        and "done-streaming" in streamed
        and streamed.index("chunk-1") < streamed.index("chunk-5"),
        repr(streamed[:160]),
    )
    results.check(
        "no gap was reported for a small stream", not gaps, f"{len(gaps)} gap(s)"
    )
    # The terminal event is why SSE was chosen over a raw byte stream: without it a client
    # cannot tell a finished command from a dropped connection. Asserted on the event
    # itself rather than only on the envelope's summary, because the summary is derived
    # from it and would agree with its own absence.
    results.check(
        "the terminal exit event carried the real exit code",
        bool(exits) and exits[-1].get("exitCode") == 0,
        repr(exits[-1] if exits else None),
    )

    # Streaming must not consume the exec: poll is a separate view onto the same
    # server-side object.
    polled = cli.call("exec", "--poll", "stream1", *attach)
    results.check(
        "the exec survived being streamed and is still pollable",
        "done-streaming" in (polled.data.get("stdout") or ""),
        repr((polled.data.get("stdout") or "")[:80]),
    )


def drive_stdin(cli: Cli, launched: Envelope, results: Results) -> None:
    """stdin: five checks. `cat` cannot exit until stdin closes, so this fails by hanging.

    That is the shape worth stating. If EOF never reaches the child, `cat` blocks until its
    timeout — which is exactly the trap where `Child::wait()` drops its own stdin handle but
    not the daemon's. So the `--timeout 30` is load-bearing: it turns a hang into a
    reported failure inside half a minute rather than at the suite's outer deadline.

    The refusal at the end is the opt-in property: a command that did not ask for stdin
    must not have one, or every task command inherits a surprise open descriptor. The daemon
    answers **409** for it (`agentd/src/exec.rs:700`) — the request is well-formed and it is
    the exec that cannot accept it — which is a different fact from the 410 a write after
    EOF gets, and the kind is what says which.
    """
    print("\n-- stdin --")
    attach = attach_args(cli, launched)

    # `exec --stdin` feeds this process's stdin and closes it. Fed through the shell rather
    # than by writing to the child's stdin from Python, so the whole path — local read,
    # chunked write, EOF on the last chunk — is the one under test.
    proc = subprocess.run(
        cli.argv(
            "exec", "cat", "--stdin", "--exec-id", "cat1", "--timeout", "30", *attach
        ),
        input="hello via stdin\n",
        capture_output=True,
        text=True,
        check=False,
        timeout=120.0,
    )
    argv = cli.argv("exec", "cat", "--stdin")
    cli.log.append(shlex.join(cli.argv("exec", "cat", "--stdin", "--exec-id", "cat1")))
    echoed = Cli.parse_stdout(proc.stdout, argv)

    results.check(
        "stdin write accepted",
        echoed.status == "ok",
        f"status={echoed.status} code={echoed.code!r}",
    )
    results.check(
        "stdin close accepted",
        echoed.status == "ok" and echoed.data.get("exitCode") is not None,
        f"exitCode={echoed.data.get('exitCode')!r}",
    )
    # The load-bearing pair. `cat` exiting at all *is* the EOF having arrived.
    results.eq(
        "a child reading stdin exits once stdin closes",
        echoed.data.get("exitCode"),
        0,
    )
    results.eq(
        "stdin round-tripped through the child",
        echoed.data.get("stdout"),
        "hello via stdin\n",
    )

    # Opt-in: an exec started without `--stdin` has /dev/null on its stdin.
    cli.call("exec", "true", "--exec-id", "nostdin", *attach)
    results.raises(
        "writing stdin to a command that did not request it is refused",
        "Conflict",
        lambda: cli.call("stdin", "nostdin", "--data", "x", *attach),
    )


def drive_suspend_resume(cli: Cli, launched: Envelope, results: Results) -> None:
    """Checks that a suspended sandbox comes back whole, driven entirely through the CLI.

    The evidence is the oracle's: a ticker writing epoch seconds once a second, and a gap in
    *its* timestamps is the suspension as the guest experienced it. The reads go through
    `microvm exec` — a shell redirect is a file write and `cat` is a file read — which is
    how this section worked before `microvm cp` existed, and it stays that way: the file
    surface has its own section now, and threading it through here would test it twice and
    the suspension no better.

    The ticker is started with a **stable id** so the pre-suspend exec record can be polled
    after the resume. That check was a documented substitute before `--poll` existed; it is
    now the assertion the oracle actually ran.
    """
    microvm_id = str(launched.data["microvmId"])
    attach = attach_args(cli, launched)

    print("\n== suspend / resume ==")
    # Detached, so the ticker's exec record is left **unacked** — which makes the
    # pre-suspend-record check below strictly stronger in two ways. An unacked entry has no
    # collection deadline at all (`agentd/src/exec.rs:214`: "an unacked entry has no deadline
    # and is never collected"), so its survival across the freeze is the daemon's registry
    # being intact rather than a 15-minute TTL not having elapsed; and its output is still
    # buffered, so the poll can assert the record came back *with* what it captured instead of
    # only that it answered.
    cli.call(
        "exec",
        "nohup sh -c 'i=0; while [ $i -lt 3000 ]; do date +%s >> /tmp/ticks.txt; "
        "i=$((i+1)); sleep 1; done' >/dev/null 2>&1 & echo started",
        "--exec-id",
        "ticker",
        "--detach",
        *attach,
    )
    cli.call("exec", "echo 'written before the suspend' > /tmp/survives.txt", *attach)
    time.sleep(5)

    print("  suspending")
    suspended = cli.call("suspend", microvm_id, "--region", cli.region)
    results.eq("suspend reached SUSPENDED", suspended.data.get("state"), "SUSPENDED")
    time.sleep(SUSPEND_WINDOW_SEC)

    print("  resuming")
    resumed = cli.call("resume", microvm_id, "--region", cli.region)
    results.eq("resume reached RUNNING", resumed.data.get("state"), "RUNNING")
    # The endpoint the service reported, which is measured not to change across a cycle.
    # Asserting it makes that measurement a fact this suite depends on rather than an
    # assumption either client encodes.
    results.eq(
        "the endpoint survived the cycle",
        resumed.data.get("endpoint"),
        launched.data.get("endpoint"),
    )

    # `resume` hands back the endpoint it read; re-attach through it rather than through
    # the launch's, so a changed endpoint is followed rather than silently failed on.
    after = attach_args(cli, launched, endpoint=str(resumed.data["endpoint"]))

    answered = None
    for _ in range(12):
        try:
            answered = cli.call("exec", "echo awake", *after)
            break
        except KindError as exc:
            print(f"    exec after resume: {exc!r}")
            time.sleep(5)
    results.check(
        "the daemon answers after a resume", answered is not None, repr(answered)
    )
    # The load-bearing one. An exec needs the installed agent token, so an exec that
    # works *is* the token having survived — if it had not, this is a 401 and every
    # consumer needs token re-delivery plumbing.
    results.check(
        "the agent token survived the suspend",
        answered is not None and answered.data.get("exitCode") == 0,
        "an authenticated exec succeeded after the resume",
    )

    survived = cli.call("exec", "cat /tmp/survives.txt", *after)
    results.eq(
        "the filesystem survived the suspend",
        (survived.data.get("stdout") or "").strip(),
        "written before the suspend",
    )

    dump = cli.call("exec", "cat /tmp/ticks.txt | tr '\\n' ' '", *after)
    stamps = [int(x) for x in (dump.data.get("stdout") or "").split() if x.isdigit()]
    gaps = [b - a for a, b in itertools.pairwise(stamps)]
    largest = max(gaps) if gaps else 0
    results.check(
        "the guest observed the suspension as a single gap in its own clock",
        largest >= 30,
        f"largest gap {largest}s across a ~{SUSPEND_WINDOW_SEC}s suspension",
    )

    # Differential liveness, the oracle's shape: two counts a few seconds apart rather than
    # a `pgrep` pattern threaded through two layers of shell quoting, where a false
    # negative is indistinguishable from a real one.
    first = cli.call("exec", "wc -l < /tmp/ticks.txt", *after)
    n1 = int((first.data.get("stdout") or "0").strip() or 0)
    time.sleep(6)
    second = cli.call("exec", "wc -l < /tmp/ticks.txt", *after)
    n2 = int((second.data.get("stdout") or "0").strip() or 0)
    results.check(
        "a backgrounded process resumed and kept running",
        n2 - n1 >= 3,
        f"ticks grew by {n2 - n1} over 6s after resume",
    )

    # The oracle's own check, restored. The ticker was started before the suspend with a
    # stable id, so polling it now asks the daemon for a record that existed on the other
    # side of a freeze — and a poll is read-only, so this costs the ticker nothing. Before
    # `--poll` existed this was a documented substitute (the ticker's *output* standing in
    # for its record); the substitute is still asserted above, and this is the real thing.
    #
    # The ticker was started `--detach`, so its record is unacked: the assertion is on the
    # *output it captured* coming back, not merely on the poll answering. An acked entry would
    # answer with an empty `stdout` and this would pass on nothing.
    survived_record = None
    try:
        survived_record = cli.call("exec", "--poll", "ticker", *after)
    except (KindError, EnvelopeError) as exc:
        results.check(
            "an exec record from before the suspend survived", False, repr(exc)
        )
    if survived_record is not None:
        results.check(
            "an exec record from before the suspend survived",
            "started" in (survived_record.data.get("stdout") or ""),
            f"phase={survived_record.data.get('phase')!r} "
            f"stdout={(survived_record.data.get('stdout') or '')[:40]!r}",
        )


def drive_token_rotation(cli: Cli, launched: Envelope, results: Results) -> None:
    """Reattach after a token rotation: gap 5 of `docs/HARNESS-CAPABILITIES.md`. Four checks.

    The contract under test is the one Harbor's hand-rolled daemon existed for: a detached
    exec must outlive the 60-minute proxy-token ceiling, because all exec state lives in the
    daemon keyed by `exec_id` and a re-minted token reattaches to it. Waiting a real hour to
    watch a token expire would cost more than every other section combined and would test
    AWS's clock, not this contract — so what is exercised is the *mechanism* the survival
    rests on: a fresh attach mints a fresh proxy token (`CoreSeam::attach_session` builds a
    new `PlaneMinter` per invocation, so every `microvm` process here is a new token), and
    the reattach carries **no client state at all** beyond the three identifiers a harness
    would have persisted. If the daemon's ack-before-TTL property held only for the process
    that started the exec, this is the section that would say so.

    The rotation is real, not simulated: each `Cli.call` is a separate process, so the
    start, the polls, and the ack below run under *different* proxy tokens by construction.
    What a 60-minute wait would add is only the proof that an **expired** token is refused,
    which is the platform's property (`microvms-core/src/session/proxy.rs:63`), not the
    daemon's or this client's.

    The output produced *before* the reattach is the assertion that matters: bytes buffered
    under token A must be readable under token B, or a harness that rotates mid-run loses
    everything its workload said before minute sixty.
    """
    print("\n-- reattach after token rotation (gap 5) --")
    attach = attach_args(cli, launched)

    # Two echoes bracketing a sleep, detached: the first lands under the starting token,
    # the second lands while the polls below are already running under later ones. The
    # sleep is long enough that the start's own process has exited — and its token with
    # it, as far as any shared state goes — before the exec finishes.
    started = cli.call(
        "exec",
        "echo before-rotation; sleep 8; echo after-rotation",
        "--exec-id",
        "rot1",
        "--detach",
        *attach,
    )
    results.eq(
        "a detached exec accepted before the rotation",
        started.data.get("phase"),
        "running",
    )

    # The reattach: a new process, a new `attach_session`, a new proxy token, and nothing
    # carried over but the three identifiers. Polled to completion the same way the
    # identity section polls, because polling is the read a reattaching harness performs.
    final = None
    for _ in range(20):
        final = cli.call("exec", "--poll", "rot1", *attach)
        if final.data.get("phase") != "running":
            break
        time.sleep(1)
    assert final is not None
    rotated_stdout = final.data.get("stdout") or ""
    results.check(
        "a reattach from only the three identifiers reads the exec",
        final.data.get("phase") == "exited" and final.data.get("exitCode") == 0,
        f"phase={final.data.get('phase')!r} exitCode={final.data.get('exitCode')!r}",
    )
    # The load-bearing one. `before-rotation` was written under the starting token and
    # nothing acked it, so it must still be in the buffer the rotated attach reads. An
    # empty or truncated-at-the-front stdout here is output lost across a rotation, which
    # is exactly what the ack-before-TTL design exists to prevent.
    results.check(
        "no output produced before the reattach was lost",
        "before-rotation" in rotated_stdout and "after-rotation" in rotated_stdout,
        repr(rotated_stdout[:80]),
    )
    # And the exec is still one exec: the ack that releases it goes through yet another
    # fresh token, and it works exactly once — proving the rotated attaches were views
    # onto the daemon's one record rather than anything token-scoped.
    results.ok(
        "the rotated session acks the exec it did not start",
        lambda: cli.call("ack", "rot1", *attach),
    )


def drive_idle_keepalive(
    cli: Cli, launched: Envelope, aws: Any, results: Results
) -> None:
    """External polling resets the idle timer: gap 6's unmeasured tail. Four checks —
    three measurements plus this section's own teardown, which is a recorded row for the
    same reason `drive_teardown`'s log-group delete is: a cleanup that quietly failed
    would leave a billing VM behind a green run.

    `docs/PLATFORM.md` measured this once by hand ("An outside poll of `/v1/health` does
    reset the idle timer") with a polled VM and an unpolled control; this is that
    measurement as a named check, so it cannot silently stop being true. Both halves run
    against **one** VM, sequentially — survive-while-polled first, suspend-once-unpolled
    second — because the second half doubles as this section's own control: a platform that
    stopped suspending idle VMs at all would pass the first half vacuously, and the second
    is what would catch it.

    Its own VM rather than the suite's, launched from the image the suite already built
    (`run --image`, so no second 15-minute build): the suite's VM carries `--max-idle-sec
    600` because every other section needs it to stay up, and running *it* to the edge of a
    10-minute window would cost more wall time than this whole file. 60 is the model's
    minimum (`IdlePolicy.maxIdleDurationSeconds` declares `min: 60`).

    This is the slow section and says so: about four minutes of deliberate waiting — ~90s
    polled, then up to ~150s waiting for the unpolled suspend — plus one VM launch. The
    teardown is in this function's own `finally`, not the caller's, because the caller's
    `finally` only knows the suite's VM; a section that launches must be the section that
    terminates, however it exits.
    """
    print("\n-- idle-timer reset via external polling (gap 6) --")
    idle_window = 60  # the model's minimum, and the whole reason this is affordable
    print(
        f"  slow check: ~4 minutes of deliberate waiting against a {idle_window}s idle window"
    )
    second = cli.call(
        "run",
        "--image",
        str(launched.data["imageIdentifier"]),
        "--name",
        f"microvm-cli-conformance-idle-{secrets.token_hex(4)}",
        "--memory",
        str(BASELINE_MEMORY_MIB),
        "--keep",
        "--region",
        cli.region,
        "--max-idle-sec",
        str(idle_window),
        "--suspended-sec",
        "600",
        "--max-duration-sec",
        "1800",
        timeout=15 * 60,
    )
    microvm_id = str(second.data["microvmId"])
    attach = attach_args(cli, second)
    # The control plane's own state read, because "still RUNNING" is the platform's claim
    # to make: a health answer alone could not distinguish a live VM from one the poll
    # itself just auto-resumed.
    plane = aws.client(SERVICE)

    try:
        # Half one: no exec traffic for 1.5x the idle window, while `microvm health` polls
        # from outside every 15 seconds — well under the window, with three missed polls of
        # margin. Each poll is one small inbound request through the endpoint proxy, which
        # is the thing the platform meters.
        deadline = time.monotonic() + idle_window * 1.5
        polls = 0
        while time.monotonic() < deadline:
            cli.call("health", *attach)
            polls += 1
            time.sleep(15)
        state = plane.get_microvm(microvmIdentifier=microvm_id)["state"]
        results.check(
            "a VM polled from outside outlives its idle window",
            state == "RUNNING",
            f"{state} after {int(idle_window * 1.5)}s against a {idle_window}s window, "
            f"{polls} health polls",
        )
        # And the poll was informed, not blind: `busy` reads false on a VM running nothing,
        # which is the field an orchestrator branches on before deciding to keep paying.
        quiet = cli.call("health", *attach)
        results.eq(
            "an idle VM reports itself not busy to its keepalive",
            quiet.data.get("busy"),
            False,
        )

        # Half two, the control: stop polling and let the window elapse. This is the half
        # that proves the first was the polling — a VM that also survived *this* would mean
        # the platform had stopped metering and the check above passed against nothing.
        # Sampled through the control plane only, because a health poll here would reset
        # the very timer being watched.
        print(f"  polling stopped; waiting for the {idle_window}s window to elapse")
        suspended_state = None
        wait_deadline = time.monotonic() + idle_window * 2.5
        while time.monotonic() < wait_deadline:
            time.sleep(20)
            suspended_state = plane.get_microvm(microvmIdentifier=microvm_id)["state"]
            if suspended_state != "RUNNING":
                break
        results.check(
            "the same VM suspends once the polling stops",
            suspended_state in {"SUSPENDING", "SUSPENDED"},
            f"{suspended_state} after the window elapsed unpolled",
        )
    finally:
        # This section's own VM, this section's own teardown. Terminate works from RUNNING,
        # SUSPENDING, and SUSPENDED alike, so however the checks above ended the VM goes.
        # No `--delete-image`: the image is the suite's and the caller's teardown owns it.
        # `data.leaked` is read rather than only "no exception", because `terminate` reports
        # a failed delete as a named leak on a success envelope — that is its contract, and
        # a check that only caught the raise would call a leaked VM torn down.
        try:
            torn = cli.call(
                "terminate", microvm_id, "--wait", "--region", cli.region, timeout=300.0
            )
        except Exception as exc:  # noqa: BLE001 - a teardown failure is a finding
            results.check(
                "the idle-check VM was terminated", False, f"{microvm_id}: {exc!r}"
            )
        else:
            results.check(
                "the idle-check VM was terminated",
                not torn.data.get("leaked"),
                f"{microvm_id} leaked={torn.data.get('leaked')!r}",
            )


def drive_teardown(
    cli: Cli, launched: Envelope, results: Results, logs: Any = None
) -> None:
    """`microvm terminate --delete-image`, what it names as left behind, and then this
    suite deleting that.

    The build log group appearing in `undeletedLogGroups` is a **normal outcome for the
    client**, not a failure: neither `microvms-core` nor the CLI carries a CloudWatch
    client, so the group is named rather than deleted. That is the whole reason it is
    named — the service created it, no Terraform stack owns it, and `terraform destroy`
    leaves it behind. Six of them accumulated before anyone noticed.

    **But naming is where the client's responsibility ends and this suite's begins, and
    it did not pick it up.** Measured 2026-08-15, once `mise run live` was fixed to run
    its leak check *after* the suite rather than beside it: five log groups from five
    conformance runs, and `scripts/verify-clean.py` calls every one a leak — correctly, since
    a service-created group nothing owns is exactly what that script exists to find. So
    the two halves of one tier disagreed by construction. `mise run live` could not be
    green on a clean account no matter what the code did, and the only stable responses to
    a gate that always fails are to stop reading it or to weaken it.

    The suite deletes its own group, which is the resolution that keeps both claims: the
    client still refuses CloudWatch (CLI-2) and still names what it cannot remove, and the
    thing that *created* the group is the thing that removes it. `logs` is the boto3 client
    already built for `read_daemon_logs` — this needs no new dependency, only for the suite
    to finish the job the report handed it.
    """
    print("\n== teardown ==")
    torn = cli.call(
        "terminate",
        str(launched.data["microvmId"]),
        "--image-identifier",
        str(launched.data["imageIdentifier"]),
        "--image-name",
        str(launched.data["imageName"]),
        "--delete-image",
        "--wait",
        "--region",
        cli.region,
        timeout=15 * 60,
    )
    results.eq("terminate emitted its teardown envelope", torn.type, "microvm.teardown")
    results.check(
        "the VM and image were deleted",
        not torn.data.get("leaked"),
        f"leaked={torn.data.get('leaked')!r}",
    )
    # Named rather than absent, which is the assertion. An empty list here would mean
    # the CLI had quietly stopped reporting a group it still cannot delete.
    undeleted = torn.data.get("undeletedLogGroups") or []
    results.check(
        "the build log group was named rather than silently left",
        bool(undeleted),
        f"{undeleted!r} — this suite deletes it below, through boto3",
    )

    if logs is None:
        results.skip(
            "the suite deleted the build log group the CLI could not",
            "no CloudWatch client was passed to drive_teardown",
        )
        return

    # The suite's own residue, removed by the suite. Asserted rather than best-effort:
    # a delete that quietly failed would put the tier back where it was, red on a leak
    # nobody meant to leave.
    deleted: list[str] = []
    failures: list[str] = []
    for group in undeleted:
        try:
            logs.delete_log_group(logGroupName=str(group))
            deleted.append(str(group))
        except Exception as exc:  # noqa: BLE001 - the reason is the finding
            # An already-absent group is the desired end state, not a failure: the
            # service may never have created one for a build that produced no events.
            if type(exc).__name__ == "ResourceNotFoundException":
                deleted.append(f"{group} (already absent)")
            else:
                failures.append(f"{group}: {type(exc).__name__}: {exc}")
    results.check(
        "the suite deleted the build log group the CLI could not",
        not failures and len(deleted) == len(undeleted),
        f"deleted={deleted!r} failures={failures!r}",
    )


def read_daemon_logs(logs: Any, image_name: str) -> list[str]:
    """The daemon's own log lines, through boto3.

    Through boto3 rather than through the CLI on purpose: `microvm logs` refuses to read
    CloudWatch by design (CLI-2), and this check is about whether the *daemon* wrote
    anything. Same shape and same reason as the oracle's own log read.
    """
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


# ── the offline self-test ───────────────────────────────────────────────────

#: A stub `microvm` that emits a canned envelope chosen by its first non-flag argument.
#:
#: A real subprocess rather than a mocked `subprocess.run`, because the thing under test
#: is the *whole* path — argv construction, stdout capture, the exit-code cross-check,
#: `json.loads` over the entire stream. A mock at the `run` boundary would skip the two
#: that have actually been wrong.
#:
#: The `stream*` cases below are the additions the flip needed, and they are why the stub
#: exists at all rather than the reader being tested against a string: `Cli.call_stream`'s
#: whole job is asserting a *multi-line stdout shape*, and the four ways that shape can be
#: wrong — envelope first, envelope pretty-printed across lines, a non-envelope last line,
#: the wrong discriminant — are only reachable by a process that really writes them.
STUB_SOURCE = '''#!/usr/bin/env python3
"""A fake `microvm` for conformance/run_rs.py --self-test. Emits canned envelopes."""
import json
import sys

STREAM_ENVELOPE = {
    "status": "ok", "apiVersion": "1", "type": "microvm.exec.stream",
    "data": {"execId": "x-1", "events": 2, "bytes": 8, "nextOffset": 8,
             "exitCode": 0, "truncated": False, "gaps": 0},
}
STREAM_EVENTS = [
    {"event": "output", "stream": "stdout", "offset": 0, "bytes": 8,
     "text": "chunk-1\\n", "lossy": False},
    {"event": "exit", "exitCode": 0, "signal": None, "truncated": False,
     "writersMayBeAlive": False, "offset": 8},
]

CASES = {
    "ok": ({"status": "ok", "apiVersion": "1", "type": "microvm.state",
            "data": {"microvmId": "mvm-1", "state": "SUSPENDED"}}, 0),
    "conflict": ({"status": "error", "apiVersion": "1", "error": "409",
                  "code": "ERR_PROTOCOL", "exitCode": 5, "finding": "",
                  "suggestions": [], "data": {"kind": "Conflict"}}, 5),
    "notfound": ({"status": "error", "apiVersion": "1", "error": "404",
                  "code": "ERR_PROTOCOL", "exitCode": 5, "finding": "",
                  "suggestions": [], "data": {"kind": "NotFound"}}, 5),
    "protocol": ({"status": "error", "apiVersion": "1", "error": "400",
                  "code": "ERR_PROTOCOL", "exitCode": 5, "finding": "",
                  "suggestions": [], "data": {"kind": "ProtocolError"}}, 5),
    "localreject": ({"status": "error", "apiVersion": "1",
                     "error": "off-table size class", "code": "ERR_INVALID_ARG",
                     "exitCode": 2, "finding": "", "suggestions": [], "data": {}}, 2),
    "leak": ({"status": "error", "apiVersion": "1", "error": "interrupted",
              "code": "ERR_INTERRUPTED", "exitCode": 11,
              "finding": "The build log group survives Terraform", "suggestions": [],
              "data": {"kind": "Conflict", "leaked": ["mvm-1", "arn:image"]}}, 11),
    "execfailed": ({"status": "ok", "apiVersion": "1", "type": "microvm.exec",
                    "data": {"execId": "x-1", "exitCode": 4}}, 13),
    # The exit code and the envelope disagree: CLI-3's claim is that they never do.
    "mismatch": ({"status": "error", "apiVersion": "1", "error": "boom",
                  "code": "ERR_PLATFORM", "exitCode": 9, "finding": "",
                  "suggestions": [], "data": {}}, 7),
    # The five attached commands, so the self-test drives the argv this suite now builds
    # for each of them rather than only the lifecycle ones.
    "health": ({"status": "ok", "apiVersion": "1", "type": "microvm.health",
                "data": {"version": "0.1.0", "bootstrapped": True,
                         "identityDegraded": False, "identityRepaired": True,
                         "diskAvailableBytes": 1024, "diskUnderPressure": False}}, 0),
    "ack": ({"status": "ok", "apiVersion": "1", "type": "microvm.exec",
             "data": {"execId": "x-1", "phase": "acked", "exitCode": 0,
                      "stdout": "released", "stderr": "", "truncated": False}}, 0),
    "poll": ({"status": "ok", "apiVersion": "1", "type": "microvm.exec",
              "data": {"execId": "x-1", "phase": "running", "exitCode": None,
                       "stdout": "", "stderr": "", "truncated": False}}, 0),
    # `exec --detach`: started, nothing waited on, nothing acked. Same envelope shape as
    # a poll of a running exec, which is the point — one `render_exec` serves both, so a
    # consumer needs one parser rather than two.
    "detach": ({"status": "ok", "apiVersion": "1", "type": "microvm.exec",
                "data": {"execId": "c1", "phase": "running", "exitCode": None,
                         "stdout": "", "stderr": "", "truncated": False}}, 0),
    # A poll of a detached exec that has since exited, with its output still buffered
    # because nothing acked it. This is the shape `polling reads an exec without
    # consuming it` reads, and the shape the first live round could not produce.
    "polldone": ({"status": "ok", "apiVersion": "1", "type": "microvm.exec",
                  "data": {"execId": "c1", "phase": "exited", "exitCode": 0,
                           "stdout": "identity-live\\n", "stderr": "",
                           "truncated": False}}, 0),
    "stdinwrite": ({"status": "ok", "apiVersion": "1", "type": "microvm.stdin",
                    "data": {"execId": "x-1", "written": 5, "eof": True}}, 0),
    "cp": ({"status": "ok", "apiVersion": "1", "type": "microvm.copy",
            "data": {"direction": "upload", "bytes": 28, "local": "./f",
                     "remote": "/tmp/f", "tar": False}}, 0),
}

args = [a for a in sys.argv[1:] if not a.startswith("--")]
case = args[0] if args else "ok"

if case == "twoenvelopes":
    print(json.dumps(CASES["ok"][0]))
    print(json.dumps(CASES["ok"][0]))
    raise SystemExit(0)
if case == "progress":
    print("building image")
    print(json.dumps(CASES["ok"][0]))
    raise SystemExit(0)
if case == "notjson":
    print("error ERR_PROTOCOL: 409")
    raise SystemExit(5)

# -- the NDJSON stream cases, and its four malformations ---------------------
if case == "stream":
    for event in STREAM_EVENTS:
        print(json.dumps(event))
    print(json.dumps(STREAM_ENVELOPE))
    raise SystemExit(0)
if case == "streamfailed":
    # A streamed exec whose workload exited non-zero: a SUCCESS envelope with a
    # non-zero code, exactly as the non-streaming case does.
    envelope = json.loads(json.dumps(STREAM_ENVELOPE))
    envelope["data"]["exitCode"] = 4
    for event in STREAM_EVENTS[:1]:
        print(json.dumps(event))
    print(json.dumps({"event": "exit", "exitCode": 4, "signal": None,
                      "truncated": False, "writersMayBeAlive": False, "offset": 8}))
    print(json.dumps(envelope))
    raise SystemExit(13)
if case == "streamenvelopefirst":
    # The envelope leading: a consumer reading line by line hits the terminator
    # before any output and concludes the command produced none.
    print(json.dumps(STREAM_ENVELOPE))
    for event in STREAM_EVENTS:
        print(json.dumps(event))
    raise SystemExit(0)
if case == "streampretty":
    # The envelope pretty-printed, so the terminating record is nine broken lines.
    for event in STREAM_EVENTS:
        print(json.dumps(event))
    print(json.dumps(STREAM_ENVELOPE, indent=2))
    raise SystemExit(0)
if case == "streamnoenvelope":
    # Events and no terminator at all.
    for event in STREAM_EVENTS:
        print(json.dumps(event))
    raise SystemExit(0)
if case == "streamwrongtype":
    # NDJSON with the NON-streaming discriminant, which is the subtle one: the shape
    # is right and a consumer branching on `type` would pick the wrong parser.
    envelope = json.loads(json.dumps(STREAM_ENVELOPE))
    envelope["type"] = "microvm.exec"
    for event in STREAM_EVENTS:
        print(json.dumps(event))
    print(json.dumps(envelope))
    raise SystemExit(0)
if case == "streamerror":
    # A stream that failed part-way: the events written stay written, and the failure
    # envelope is the last line.
    print(json.dumps(STREAM_EVENTS[0]))
    print(json.dumps({"status": "error", "apiVersion": "1", "error": "cut",
                      "code": "ERR_RETRYABLE", "exitCode": 3, "finding": "",
                      "suggestions": [], "data": {"kind": "Transport"}}))
    raise SystemExit(3)

document, code = CASES[case]
print(json.dumps(document))
sys.exit(code)
'''


def self_test() -> int:
    """Drives the envelope-to-exception mapping against the stub. No AWS, no money.

    The point is not that the mapping works — it is that **`Results.raises`
    discriminates**. A `raises` that passed for any failure at all would make the five
    `ERR_PROTOCOL` checks vacuous, and vacuous is exactly how they would look green. So
    every positive case here has its negative twin, and the negatives are asserted to
    FAIL rather than described as failing.
    """
    print("== self-test: the envelope→exception mapping, offline ==")
    with tempfile.TemporaryDirectory() as tmp:
        stub = Path(tmp) / "microvm"
        stub.write_text(STUB_SOURCE)
        stub.chmod(stub.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)
        cli = Cli(binary=stub)
        results = Results()

        # -- the success side -------------------------------------------------
        ok = cli.call("ok")
        results.eq(
            "a success envelope parses its discriminant", ok.type, "microvm.state"
        )
        results.eq(
            "a success envelope carries its data", ok.data.get("state"), "SUSPENDED"
        )
        results.check("a success envelope has no kind", ok.kind is None, repr(ok.kind))

        # -- the load-bearing one --------------------------------------------
        # Three failures that are IDENTICAL in code and exit code and differ only in
        # `data.kind`. This is the whole reason this driver asserts on kinds: if the
        # coarse code were enough, these three would be one check.
        kinds = {}
        for case in ("conflict", "notfound", "protocol"):
            try:
                cli.call(case)
            except KindError as exc:
                kinds[case] = (exc.kind, exc.code, exc.exit_code)
        results.check(
            "three protocol failures share one code and one exit code",
            {v[1] for v in kinds.values()} == {"ERR_PROTOCOL"}
            and {v[2] for v in kinds.values()} == {5},
            repr(kinds),
        )
        results.check(
            "and are distinguishable only through data.kind",
            {v[0] for v in kinds.values()} == {"Conflict", "NotFound", "ProtocolError"},
            repr({k: v[0] for k, v in kinds.items()}),
        )

        # -- raises() asserts on the kind, positively and negatively ----------
        results.raises(
            "raises matches the expected kind", "Conflict", lambda: cli.call("conflict")
        )
        results.raises(
            "raises matches NotFound", "NotFound", lambda: cli.call("notfound")
        )

        # The negative twins, run through a throwaway Results so their failures are the
        # assertion rather than this run's verdict. A guard that cannot be made to fail
        # is not a guard, and these are the three ways it could be vacuous. Printed as
        # `PROBE` because a green run must not print a line that reads as broken.
        print(
            "  -- probing that raises() can fail (each PROBE line below is expected) --"
        )
        probe = Results(probe=True)
        probe.raises("wrong kind must fail", "Conflict", lambda: cli.call("notfound"))
        probe.raises("nothing raised must fail", "Conflict", lambda: cli.call("ok"))
        probe.raises(
            "a local reject has no kind", "Conflict", lambda: cli.call("localreject")
        )
        results.eq(
            "raises() fails on a wrong kind, no raise, and an absent kind",
            len(probe.failed),
            3,
        )
        results.eq("and passes nothing while doing it", len(probe.passed), 0)

        # -- the local reject carries no kind, which is information ------------
        try:
            cli.call("localreject")
        except KindError as exc:
            results.check(
                "a local reject reports no wire kind",
                exc.kind is None
                and exc.code == "ERR_INVALID_ARG"
                and exc.exit_code == 2,
                repr(exc),
            )

        # -- CLI-6's envelope half --------------------------------------------
        try:
            cli.call("leak")
        except KindError as exc:
            results.eq(
                "leaked identifiers and the wire kind coexist in data",
                (exc.envelope.data.get("leaked"), exc.kind),
                (["mvm-1", "arn:image"], "Conflict"),
            )
            results.check(
                "a platform-trap failure names its finding",
                exc.envelope.finding == "The build log group survives Terraform",
                repr(exc.envelope.finding),
            )

        # -- the success-envelope-with-non-zero-exit case ----------------------
        # `already_reported`: the payload is right and the exit code is not zero. It must
        # NOT raise, because the caller asked for the output and the output is there.
        results.ok("a failing workload does not raise", lambda: cli.call("execfailed"))

        # -- CLI-4, three ways it can break ----------------------------------
        for case, why in (
            ("twoenvelopes", "two envelopes on stdout"),
            ("progress", "a progress line on stdout"),
            ("notjson", "human text on stdout"),
        ):
            try:
                cli.call(case)
            except EnvelopeError:
                results.check(f"CLI-4: {why} is caught", True)
            except KindError as exc:
                results.check(
                    f"CLI-4: {why} is caught",
                    False,
                    f"read as a protocol result: {exc!r}",
                )
            else:
                results.check(
                    f"CLI-4: {why} is caught", False, "parsed as one envelope"
                )

        # -- CLI-3: the two renderings of one decision must agree -------------
        try:
            cli.call("mismatch")
        except EnvelopeError as exc:
            results.check(
                "CLI-3: a $? that disagrees with exitCode is caught",
                True,
                str(exc)[:80],
            )
        except KindError:
            results.check(
                "CLI-3: a $? that disagrees with exitCode is caught",
                False,
                "the disagreement was accepted",
            )

        # -- the NDJSON stream reader -----------------------------------------
        #
        # `Cli.call_stream` is the one function in this file with a contract of its own, so
        # it gets the same treatment `Results.raises` got above: the happy path, then every
        # way it can be vacuous. A reader that accepted any multi-line stdout would make
        # all five streaming checks pass against a CLI that had stopped streaming.
        events, envelope = cli.call_stream("stream")
        results.eq("a stream yields its events and its envelope", len(events), 2)
        results.eq(
            "a stream's envelope carries the streaming discriminant",
            envelope.type,
            "microvm.exec.stream",
        )
        results.check(
            "a stream's events are events rather than envelopes",
            all("status" not in event for event in events)
            and events[0].get("event") == "output"
            and events[-1].get("event") == "exit",
            repr([event.get("event") for event in events]),
        )
        results.eq(
            "a stream's summary reports the event count",
            envelope.data.get("events"),
            2,
        )

        # A streamed exec whose workload failed keeps its success envelope and a non-zero
        # `$?` — the `already_reported` case on the streaming path, which `call_stream` must
        # not raise on for the same reason `call` must not.
        results.ok(
            "a failing streamed workload does not raise",
            lambda: cli.call_stream("streamfailed"),
        )

        print(
            "  -- probing that call_stream() can fail (each PROBE line below is expected) --"
        )
        stream_probe = Results(probe=True)
        for case, why in (
            ("streamenvelopefirst", "the envelope written first"),
            ("streampretty", "a pretty-printed envelope spanning lines"),
            ("streamnoenvelope", "no envelope at all"),
            ("streamwrongtype", "the non-streaming discriminant"),
        ):
            try:
                cli.call_stream(case)
            except EnvelopeError as exc:
                stream_probe.check(f"caught: {why}", False, str(exc)[:70])
            else:
                stream_probe.check(
                    f"NOT caught: {why}", True, "accepted a broken shape"
                )
        results.eq(
            "call_stream() rejects all four malformed stream shapes",
            len(stream_probe.failed),
            4,
        )
        results.eq("and accepts none of them", len(stream_probe.passed), 0)

        # A stream that failed mid-way raises with its kind, and the events already written
        # are not the driver's problem — the failure is.
        results.raises(
            "a mid-stream failure raises with the daemon's kind",
            "Transport",
            lambda: cli.call_stream("streamerror"),
        )

        # -- the five attached commands' argv round-trips ----------------------
        #
        # Cheap, and it covers the one thing the live tier discovers expensively: an argv
        # this suite builds that the CLI does not accept. The stub answers by its first
        # non-flag argument, so what is exercised here is `Cli.call`'s construction and the
        # envelope shape each section reads — not the CLI's parser, which `tests/manifest.rs`
        # covers at the process boundary.
        for case, kind, key in (
            ("health", "microvm.health", "bootstrapped"),
            ("ack", "microvm.exec", "phase"),
            ("poll", "microvm.exec", "phase"),
            ("stdinwrite", "microvm.stdin", "written"),
            ("cp", "microvm.copy", "direction"),
        ):
            got = cli.call(case)
            results.check(
                f"the {case} envelope parses with its own discriminant",
                got.type == kind and key in got.data,
                f"type={got.type!r} keys={sorted(got.data)}",
            )
        # A running exec's poll: `exitCode` is present and null, which is the shape
        # `--poll`'s "polling is not a failure" contract produces. Asserted because a
        # *missing* key and a null one read the same way in a permissive consumer, and this
        # suite's identity section branches on it.
        polled = cli.call("poll")
        results.check(
            "a running exec polls as a success with a present-but-null exit code",
            "exitCode" in polled.data and polled.data["exitCode"] is None,
            repr(polled.data),
        )

        # -- the start/poll/ack decomposition `--detach` restores ---------------
        #
        # The shape the first live round could not produce. `exec` without `--detach` acks
        # its own output, so `ack accepted` got a 409 and `polling reads an exec without
        # consuming it` read `''`. What is checked here is that the driver's *reading* of the
        # three-step sequence is right — a detached start reports `running` with no verdict,
        # a later poll finds the exec exited with its output still buffered, and only then is
        # there anything for an ack to release.
        detached = cli.call("detach")
        results.check(
            "a detached start reports running with no verdict yet",
            detached.data.get("phase") == "running"
            and detached.data.get("exitCode") is None
            and detached.data.get("execId") == "c1",
            repr(detached.data),
        )
        done = cli.call("polldone")
        results.check(
            "a detached exec's output is still readable when it exits",
            done.data.get("phase") == "exited"
            and "identity-live" in (done.data.get("stdout") or ""),
            repr(done.data),
        )
        # And the loop condition the identity section uses to wait for it: `phase != running`
        # is the exit test, so a `running` poll must not satisfy it and an `exited` one must.
        results.check(
            "the poll loop's exit condition distinguishes running from exited",
            cli.call("poll").data.get("phase") == "running"
            and done.data.get("phase") != "running",
            "running poll keeps looping, exited poll breaks",
        )

        # -- the four hostile archives really are hostile ----------------------
        #
        # Offline, and worth having offline: these are built with `tarfile` precisely
        # because GNU tar sanitizes them, and an archive that had been silently sanitized
        # would make four live checks pass against nothing. So the *bytes* are inspected
        # here — with `tarfile` reading them back, which is the only reader that can see a
        # member type — before any of them is ever handed to a real daemon.
        import io
        import tarfile

        archives = dict(build_hostile_archives())
        results.eq("all four hostile archives are built", len(archives), 4)

        def members(name: str) -> list[tarfile.TarInfo]:
            with tarfile.open(fileobj=io.BytesIO(archives[name]), mode="r") as tar:
                return list(tar.getmembers())

        traversal = members("parent traversal")
        results.check(
            "the traversal archive really escapes the root",
            any(".." in member.name for member in traversal),
            repr([member.name for member in traversal]),
        )
        absolute = members("absolute link target")
        results.check(
            "the absolute-link archive really names an absolute target",
            any(
                member.issym() and member.linkname.startswith("/")
                for member in absolute
            ),
            repr([(member.name, member.linkname) for member in absolute]),
        )
        redirect = members("symlink redirect")
        results.check(
            "the redirect archive is a link to .. plus a file through it",
            any(member.issym() and member.linkname == ".." for member in redirect)
            and any(member.isfile() for member in redirect),
            repr([(member.name, member.type) for member in redirect]),
        )
        device = members("character device")
        results.check(
            "the device archive really carries a character device",
            any(member.ischr() for member in device),
            repr([(member.name, member.type) for member in device]),
        )

        # -- the skip primitive still works, with no live caller ---------------
        #
        # `Results.skip` has no caller in the live path any more: the 34 entries it used to
        # print became real checks. Exercised here against a throwaway `Results` so it
        # cannot rot into a function that no longer runs — the next inexpressible check
        # needs somewhere to be recorded, and a primitive nothing ever calls is one nobody
        # notices has broken.
        skip_probe = Results()
        skip_probe.skip("a future gap", "recorded rather than silent")
        results.eq(
            "the skip primitive records rather than passing", len(skip_probe.skipped), 1
        )
        results.eq("and does not count as a pass", len(skip_probe.passed), 0)

        print("\n== self-test summary ==")
        print(f"  passed: {len(results.passed)}")
        print(f"  failed: {len(results.failed)}")
        for name, detail in results.failed:
            print(f"    FAIL {name}: {detail}")
        return 0 if not results.failed else 1


# ── entry point ─────────────────────────────────────────────────────────────


def sh(cmd: list[str], cwd: Path | None = None) -> str:
    proc = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, check=False)
    if proc.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd)} failed:\n{proc.stdout}\n{proc.stderr}")
    return proc.stdout


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="drive the envelope→exception mapping against a stub. Offline and free",
    )
    parser.add_argument(
        "--binary", type=Path, help="the aarch64 agentd binary to bake in"
    )
    parser.add_argument(
        "--microvm-binary",
        type=Path,
        default=Path("target/release/microvm"),
        help="the `microvm` CLI under test",
    )
    parser.add_argument(
        "--keep", action="store_true", help="skip teardown (leaks resources)"
    )
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    if args.binary is None:
        print("--binary is required for a live run (or pass --self-test)")
        return 2

    repo = Path(__file__).resolve().parent.parent
    infra = repo / "conformance" / "infra"
    binary = (
        (repo / args.binary).resolve() if not args.binary.is_absolute() else args.binary
    )
    microvm = (
        (repo / args.microvm_binary).resolve()
        if not args.microvm_binary.is_absolute()
        else args.microvm_binary
    )
    for label, path in (("agentd binary", binary), ("microvm CLI", microvm)):
        if not path.exists():
            print(f"{label} not found: {path}")
            return 2

    # The same three Terraform outputs the oracle read, handed to the CLI through the
    # environment rather than as flags: `MICROVM_BUCKET` and the two role ARNs are the
    # names `seam.rs:324` resolves, so this is how a human runs it too.
    outputs = json.loads(sh(["terraform", "output", "-json"], cwd=infra))
    os.environ["MICROVM_BUCKET"] = outputs["s3_bucket"]["value"]
    os.environ["MICROVM_BUILD_ROLE_ARN"] = outputs["build_role_arn"]["value"]
    os.environ["MICROVM_EXECUTION_ROLE_ARN"] = outputs["execution_role_arn"]["value"]
    print(f"infra: bucket={os.environ['MICROVM_BUCKET']}")

    cli = Cli(binary=microvm)
    results = Results()
    launched: Envelope | None = None
    daemon: Daemon | None = None
    # Built here rather than inside the `try`, because the teardown in the `finally` needs a
    # CloudWatch client and a name bound inside the block it is cleaning up after is a
    # `NameError` waiting for the one run that fails early — which would replace a real
    # failure with this file's own. Creating a boto3 session costs no API call.
    aws = boto3.Session(region_name=cli.region)

    with tempfile.TemporaryDirectory() as tmp:
        dockerfile = Path(tmp) / "Dockerfile"
        dockerfile.write_text(conformance_dockerfile(resolve_base_ref()))

        try:
            # `record_unsupported(results)` was here, printing 34 SKIP lines before
            # anything was launched so a reader knew what the run would not tell them
            # *while* it spent money. Every one of those became a real check when the CLI
            # grew the five surfaces `docs/CLI-COVERAGE-PLAN.md` names, so there is nothing
            # left to announce. The summary still prints a skip count, which should read
            # zero — see `Results.skip`.
            drive_local_commands(cli, results)
            launched = drive_lifecycle(cli, binary, dockerfile, results)

            # The daemon lane pokes the VM the CLI launched, over raw HTTP. Composed
            # rather than duplicated: the Rust client launched it and this reaches
            # around every client, which is the honest division of labour for six
            # checks that are about neither one.
            daemon = Daemon(
                endpoint=str(launched.data["endpoint"]),
                agent_token=str(launched.data["agentToken"]),
                microvm_id=str(launched.data["microvmId"]),
                microvm_client=aws.client(SERVICE),
            )
            drive_daemon_lane(daemon, results)

            drive_exec(cli, launched, results)
            drive_health(cli, launched, results)
            drive_exec_identity(cli, launched, results)
            # After the identity section because it leans on the same detach/poll/ack
            # surface that section just proved, so a rotation failure here points at the
            # rotation rather than at a broken poll.
            drive_token_rotation(cli, launched, results)
            drive_streaming(cli, launched, results)
            drive_stdin(cli, launched, results)
            drive_file_transfer(cli, launched, results, Path(tmp))
            # The cap trio *after* the file and stream sections, deliberately: it pushes
            # 32 MiB through the guest and asserts the daemon survived, so anything that
            # ran before it is evidence the survival claim is about a daemon that was
            # already doing real work — and anything after it would be confounded by it.
            drive_output_cap(cli, launched, results)
            # Suspend/resume last among the shared-VM sections, because it is the only one
            # that changes the VM's state for forty seconds and every section above wants a
            # running one.
            drive_suspend_resume(cli, launched, results)
            # The idle-keepalive section runs on its own VM (launched from the image this
            # suite already built, so no second build) and is the slowest section here —
            # its own output says how long. Last, so its four minutes of deliberate
            # waiting delay nothing, and so a failure in any cheaper section is reported
            # before this one spends its time.
            drive_idle_keepalive(cli, launched, aws, results)

            print("\n== daemon logs ==")
            lines = read_daemon_logs(
                aws.client("logs"), str(launched.data["imageName"])
            )
            results.check(
                "daemon logs reached CloudWatch under /aws/lambda-microvms/",
                bool(lines),
                f"{len(lines)} lines",
            )
        finally:
            if daemon is not None:
                daemon.close()
            if args.keep:
                print("\n== teardown SKIPPED (--keep) ==")
            elif launched is None:
                print("\n== teardown: nothing was launched ==")
            else:
                # Never raises out of here: an exception in teardown would replace the
                # real failure with a teardown failure. The log group is handled LAST
                # because the service can recreate a group deleted before its image —
                # which is how six of them leaked.
                #
                # `aws` is bound before the `try` for this call site's sake: a session
                # created inside the block would be a `NameError` here on the one run that
                # failed early, which would replace a real failure with this file's own.
                try:
                    drive_teardown(cli, launched, results, aws.client("logs"))
                except Exception as exc:  # noqa: BLE001 - a teardown failure is a finding
                    results.check("teardown completed", False, repr(exc))

    print("\n== summary ==")
    print(f"  passed:  {len(results.passed)}")
    print(f"  failed:  {len(results.failed)}")
    print(f"  skipped: {len(results.skipped)}")
    for name, detail in results.failed:
        print(f"    FAIL {name}: {detail}")
    for name, reason in results.skipped:
        print(f"    SKIP {name}: {reason}")

    # `expressed` counts every check that ran either way, so the denominator is what this
    # suite *attempted* rather than what it managed. A failing check is still an expressed
    # one — the coverage claim and the pass/fail verdict are different facts, and folding
    # them would make a red run look like a narrower suite.
    expressed = len(results.passed) + len(results.failed)
    total = expressed + len(results.skipped)
    print(
        f"\n  {expressed} of {total} named checks are expressible through this client."
    )
    if results.skipped:
        # Never reached today, and the branch stays: the moment a surface goes away or a
        # check becomes inexpressible again, this is the line that says so instead of the
        # count quietly shrinking.
        print(
            f"  {len(results.skipped)} are not, and each is named above with the surface "
            "that would have to grow."
        )
    else:
        print(
            "  The 34 the deleted Python oracle alone could reach — file transfer, tar "
            "round trips,\n  the four hostile archives, SSE ordering, the stdin lifecycle, "
            "double-ack, the 8 MiB cap\n  trio, the identity-repair flags — are live checks "
            "now, under the names run.py gave them.\n  This report diffs line for line "
            "against the last oracle run in git history."
        )
    print("\n  every invocation, for reproducing a failure by hand:")
    for line in cli.log:
        print(f"    {line}")
    return 0 if not results.failed else 1


if __name__ == "__main__":
    sys.exit(main())
