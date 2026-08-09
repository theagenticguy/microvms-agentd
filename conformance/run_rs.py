#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["boto3>=1.40", "httpx>=0.27"]
# ///
# SPDX-License-Identifier: Apache-2.0
"""Live conformance run driving the **Rust** client stack through the `microvm` CLI.

This is now the only live suite. `conformance/run.py` was the oracle — 56 checks
through the Python client — and it went away with that client once both suites ran
green against real AWS on the same commit (Python 56/56, this one 38/38). Every
check name here is still byte-identical to the name run.py gave it, and the
`UNSUPPORTED` list below still names all 34 checks only that client could express.
Both facts are kept deliberately: they are what lets a reader of this report diff it
against the last recorded oracle run in git history, and what makes the coverage this
client does not have a statement rather than a silence.

A hybrid driver, and each of the three lanes is deliberate
---------------------------------------------------------

1. **The CLI, through `--json` envelopes.** The client under test. Lifecycle
   (`run`, `exec`, `suspend`, `resume`, `terminate`), the local commands, the
   envelope contract, and the exit-code table. Every invocation also verifies CLI-4
   for free: `Cli.call` parses the whole of stdout as one JSON document, so a stray
   `println!` anywhere in the Rust crate turns this suite red rather than being
   noticed by nobody.

2. **Raw `httpx`, for six checks that test the DAEMON.** The raw run-hook POST and
   the raw status-code sends. These are the two reach-arounds `conformance/run.py`
   documented before it was deleted: the only callers of `/run` are the platform
   itself and an attacker inside the VM, and the other four assert on a status
   integer the daemon chose. They are not about the client, so the client they go
   through does not matter — and adding a raw-request escape to the CLI so they
   could go through it would violate CLI-2 and CLI-5 to make a report look tidier.

   Raw rather than through a client library, and that is the *stronger* shape for
   what these six mean. They assert on the status integer directly — 409, 200, 401,
   400 — where the deleted Python suite asserted on the exception its own taxonomy
   mapped that integer to. One layer fewer between the daemon's decision and the
   assertion about it, and no way for a client's status table to be the thing that
   passes.

3. **`unsupported()`, for the checks this CLI has no subcommand for.** Named, listed,
   and counted, never quietly dropped. See the next section, because this lane is
   larger than it looks and pretending otherwise would be the failure mode.

What this suite does NOT cover, and why that is a property rather than a bug
--------------------------------------------------------------------------

`microvms-core` carries the whole session surface — `upload_file`, `download_tar`,
`stream`, `write_stdin`, `ack`, `poll`. The **CLI** exposes almost none of it: its
`exec` is one-shot `run_sync` (start, wait, ack) with a generated exec id, and there
is no `microvm cp`, no `microvm ack`, no `microvm exec --stream`, no `microvm stdin`.

So the protocol-detail half of the old oracle — file transfer, tar round trips,
hostile archives, SSE ordering, stdin lifecycle, double-ack, the 8 MiB output cap,
the identity-repair health flags — is **not expressible through this client**, and
every one of those checks is recorded `SKIP` with the reason. That is the honest
report, and the alternative was worse in a specific way: reaching for a second client
so those checks could go green would produce a green run over a Rust stack that was
never asked to do them, which is the same false assurance
`scripts/check-model-drift` exists to refuse ("a checker that reports clean while a
constraint has drifted is worse than no checker").

What is left is exactly the half only this client can answer: the lifecycle, the
suspend/resume evidence, the teardown ordering, and the CLI's own requirements —
CLI-3's exit codes, CLI-4's one envelope, CLI-6's named leak on interrupt. Those
never existed in the oracle at all. A `SKIP` here becomes a `PASS` the day the CLI
grows the subcommand, and until then the SSE and tar surfaces are covered locally by
`microvms-core`'s own tiers rather than against real AWS — which is a real gap, named
here rather than papered over.

Money
-----

This run creates real MicroVMs and is billable, ~15 min. It belongs to
`mise run live` and is never hooked. `--self-test` is the offline half: it drives the
envelope-to-exception mapping against a stub `microvm` script and touches no account.

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

    The four primitives are the oracle's, with the same names and the same semantics, plus
    `unsupported`. `skipped` is a *third* list rather than a pass with a note, because
    a skip folded into `passed` is how a suite that covers half of what it claims looks
    identical to one that covers all of it.
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

    def unsupported(self, name: str, reason: str) -> None:
        """Records a check this client has no way to express.

        Printed as `SKIP` and counted apart from both passes and failures. It does not
        fail the run — the CLI genuinely has no subcommand for it, and a suite that is
        permanently red is a suite people stop reading — but it is never silent, which
        is the whole difference between a coverage statement and a gap.
        """
        self.skipped.append((name, reason))
        print(f"  SKIP  {name} — {reason}")


# ── the checks the CLI has no subcommand for ─────────────────────────────────

#: `(check name, reason)` for every oracle check this client cannot express.
#:
#: Written out rather than derived by diffing the oracle at runtime, and the reason was
#: the measure-coupling lesson: a runtime diff would have silently shrunk the moment the
#: oracle renamed a check, reporting better coverage for a suite that had not changed. It
#: is now the *only* record of those 34 names, which is the second reason it is a literal
#: list: a derived one would have vanished with the file it derived from.
#:
#: Each reason names the missing subcommand rather than saying "unsupported", because
#: the actionable half is which surface would have to grow.
UNSUPPORTED: tuple[tuple[str, str], ...] = (
    (
        "ack accepted",
        "no `microvm ack`: the CLI's exec is one-shot run_sync, which acks itself",
    ),
    (
        "second ack refused with 409",
        "no `microvm ack`, so a double-ack cannot be issued",
    ),
    (
        "unknown exec id is 404",
        "no `microvm exec --poll <id>`: exec ids are generated, not named",
    ),
    (
        "retried start did not spawn a second child",
        (
            "no stable --exec-id: the CLI mints one per invocation (TRAP-1's shape), so "
            "the idempotency-key retry cannot be replayed"
        ),
    ),
    ("retried start accepted", "no stable --exec-id; see the check above"),
    (
        "noisy command still exits 0",
        "expressible, but the 8 MiB cap assertion below is not",
    ),
    (
        "output past the cap was truncated",
        "`truncated` is in the exec envelope, but see below",
    ),
    ("daemon survived the truncation", "no `microvm health`"),
    ("single file write accepted", "no `microvm cp`: file transfer is core-only"),
    ("single file read returns the bytes", "no `microvm cp`"),
    ("read of an absent file is 404", "no `microvm cp`"),
    (
        "tree created for the round trip",
        (
            "expressible through `microvm exec`, but the round trip it sets up is not — "
            "so building the tree would assert nothing"
        ),
    ),
    ("tar download succeeded", "no `microvm cp --tar`"),
    ("tar upload accepted", "no `microvm cp --tar`"),
    ("symlink survived the round trip as a symlink", "no `microvm cp --tar`"),
    ("symlink still resolves to its target's content", "no `microvm cp --tar`"),
    (
        "identity repair completed every step",
        "no `microvm health`: identity_degraded is a health flag",
    ),
    (
        "identity repair actually ran",
        "no `microvm health`: identity_repaired is a health flag",
    ),
    ("SSE reached us through the endpoint proxy", "no `microvm exec --stream`"),
    ("streamed output is complete and ordered", "no `microvm exec --stream`"),
    ("no gap was reported for a small stream", "no `microvm exec --stream`"),
    (
        "the terminal exit event carried the real exit code",
        "no `microvm exec --stream`",
    ),
    (
        "the exec survived being streamed and is still pollable",
        "no `microvm exec --stream`",
    ),
    ("stdin write accepted", "no `microvm stdin`"),
    ("stdin close accepted", "no `microvm stdin`"),
    ("a child reading stdin exits once stdin closes", "no `microvm exec --stdin`"),
    ("stdin round-tripped through the child", "no `microvm exec --stdin`"),
    (
        "writing stdin to a command that did not request it is refused",
        "no `microvm stdin`, and the CLI never sets stdin:true",
    ),
    (
        "nothing escaped the extraction root",
        "no `microvm cp --tar` to extract a hostile archive",
    ),
)

#: The four hostile archives, by the name the oracle gave each. Listed so the skip report
#: names them individually — "hostile archives are not covered" is a sentence a reader
#: cannot act on, and four named members is the same fact they can.
HOSTILE_ARCHIVES = (
    "parent traversal",
    "absolute link target",
    "symlink redirect",
    "character device",
)


def record_unsupported(results: Results) -> None:
    """Every inexpressible check, printed once, before anything is launched.

    Before rather than after, so a reader watching the run knows what it is not going
    to tell them *while* it is spending money — not in a summary they reach fifteen
    minutes later.
    """
    print("\n== not expressible through this client ==")
    for name, reason in UNSUPPORTED:
        results.unsupported(name, reason)
    for archive in HOSTILE_ARCHIVES:
        results.unsupported(
            f"hostile archive refused: {archive}",
            "no `microvm cp --tar`: an archive cannot be handed to this client",
        )


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
    # A launch that returned at all means the CLI's `wait_until_ready` saw a bootstrapped
    # daemon, since that call is what it waits on. A weaker assertion than the oracle's
    # direct `health()` — it cannot observe the pre-bootstrap state — and it is the
    # strongest one available without a `microvm health`.
    results.check(
        "health reachable through the endpoint",
        bool(launched.data.get("endpoint")),
        f"endpoint {launched.data.get('endpoint')!r} (implied by wait_until_ready)",
    )
    results.check(
        "platform ran the run hook before forwarding traffic",
        launched.data.get("execExitCode") is not None,
        "an exec answered, so the token was installed before external traffic",
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


def drive_exec(cli: Cli, launched: Envelope, results: Results) -> None:
    """`microvm exec` against the kept VM — the attach path, which `run` never exercises.

    Worth its own section: `exec` goes through `attach_session` rather than
    `open_sandbox`, so it is the only command that mints a proxy token for a VM this
    process did not launch. TRAP-9 lives on that path.
    """
    print("\n-- exec (the attach path) --")
    attach = [
        "--endpoint",
        str(launched.data["endpoint"]),
        "--agent-token",
        str(launched.data["agentToken"]),
        "--microvm-id",
        str(launched.data["microvmId"]),
        "--region",
        cli.region,
    ]

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


def drive_suspend_resume(cli: Cli, launched: Envelope, results: Results) -> None:
    """Checks that a suspended sandbox comes back whole, driven entirely through the CLI.

    The evidence is the oracle's: a ticker writing epoch seconds once a second, and a gap in
    *its* timestamps is the suspension as the guest experienced it. Every read here goes
    through `microvm exec` rather than `upload_file`/`download_file`, which is why this
    section survives the CLI's missing file surface at all — a shell redirect is a file
    write, and `cat` is a file read.
    """
    microvm_id = str(launched.data["microvmId"])
    attach = [
        "--endpoint",
        str(launched.data["endpoint"]),
        "--agent-token",
        str(launched.data["agentToken"]),
        "--microvm-id",
        microvm_id,
        "--region",
        cli.region,
    ]

    print("\n== suspend / resume ==")
    cli.call(
        "exec",
        "nohup sh -c 'i=0; while [ $i -lt 3000 ]; do date +%s >> /tmp/ticks.txt; "
        "i=$((i+1)); sleep 1; done' >/dev/null 2>&1 & echo started",
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
    after = [
        "--endpoint",
        str(resumed.data["endpoint"]),
        "--agent-token",
        str(launched.data["agentToken"]),
        "--microvm-id",
        microvm_id,
        "--region",
        cli.region,
    ]

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

    # An exec record from before the suspend cannot be polled — no `microvm exec --poll`
    # — so the ticker's *output* standing in is the honest substitute, and it is named
    # as a substitute rather than as the check the oracle ran.
    results.unsupported(
        "an exec record from before the suspend survived",
        "no `microvm exec --poll <id>`; the ticker's own output is checked instead",
    )


def drive_teardown(cli: Cli, launched: Envelope, results: Results) -> None:
    """`microvm terminate --delete-image`, and what it honestly reports as left behind.

    The build log group appearing in `undeletedLogGroups` is a **normal outcome**, not a
    failure: neither `microvms-core` nor the CLI carries a CloudWatch client, so the
    group is named rather than deleted. That is the whole reason it is named — the
    service created it, no Terraform stack owns it, and `terraform destroy` leaves it
    behind. Six of them accumulated before anyone noticed.
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
    results.check(
        "the build log group was named rather than silently left",
        bool(torn.data.get("undeletedLogGroups")),
        f"{torn.data.get('undeletedLogGroups')!r} — delete with `aws logs delete-log-group`",
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
STUB_SOURCE = '''#!/usr/bin/env python3
"""A fake `microvm` for conformance/run_rs.py --self-test. Emits canned envelopes."""
import json
import sys

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

    with tempfile.TemporaryDirectory() as tmp:
        dockerfile = Path(tmp) / "Dockerfile"
        dockerfile.write_text(conformance_dockerfile(resolve_base_ref()))

        try:
            record_unsupported(results)
            drive_local_commands(cli, results)
            launched = drive_lifecycle(cli, binary, dockerfile, results)

            # The daemon lane pokes the VM the CLI launched, over raw HTTP. Composed
            # rather than duplicated: the Rust client launched it and this reaches
            # around every client, which is the honest division of labour for six
            # checks that are about neither one.
            aws = boto3.Session(region_name=cli.region)
            daemon = Daemon(
                endpoint=str(launched.data["endpoint"]),
                agent_token=str(launched.data["agentToken"]),
                microvm_id=str(launched.data["microvmId"]),
                microvm_client=aws.client(SERVICE),
            )
            drive_daemon_lane(daemon, results)

            drive_exec(cli, launched, results)
            drive_suspend_resume(cli, launched, results)

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
                # real failure with a teardown failure. The log group is reported LAST
                # because the service can recreate a group deleted before its image —
                # which is how six of them leaked.
                try:
                    drive_teardown(cli, launched, results)
                except Exception as exc:  # noqa: BLE001 - a teardown failure is a finding
                    results.check("teardown completed", False, repr(exc))

    print("\n== summary ==")
    print(f"  passed:  {len(results.passed)}")
    print(f"  failed:  {len(results.failed)}")
    print(f"  skipped: {len(results.skipped)} (no subcommand on this client)")
    for name, detail in results.failed:
        print(f"    FAIL {name}: {detail}")
    print(
        f"\n  {len(results.passed)} of {len(results.passed) + len(results.skipped)} named checks "
        "are expressible through this client. The rest were the deleted Python\n"
        "  oracle's, and no live suite covers them now — see this file's docstring."
    )
    print("\n  every invocation, for reproducing a failure by hand:")
    for line in cli.log:
        print(f"    {line}")
    return 0 if not results.failed else 1


if __name__ == "__main__":
    sys.exit(main())
