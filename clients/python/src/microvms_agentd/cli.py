"""`microvm`: a working sandbox in one command, and nothing the library does not do.

Two audiences share one package. A consumer building a product imports
`microvms_agentd`; a consumer who wants a VM to run a test suite in *now* runs
`microvm run ./image --exec pytest`. This module is the second door onto the first
room — it parses, it renders, and it exits with a code. Every AWS call and every
trap guard belongs to the library, and that is a checked property rather than an
intention: `tests/test_cli.py` asserts both that no module here imports a
transport and that patching the library's entry points to raise makes every
command fail. Either guard alone is defeatable, which is why there are two.

A coding agent is a first-class consumer, so the surface is machine-legible by
construction: `microvm manifest` emits the whole command tree with its option
domains, exit codes, and envelope schema; every command honors `--json` and emits
exactly one envelope object on stdout with nothing else on that stream. Progress
goes to stderr, always, because a CLI that writes a log line to stdout passes an
"is the envelope there" check and breaks the parse.
"""

from __future__ import annotations

import contextlib
import inspect
import json as jsonlib
import os
import shutil
import subprocess
import sys
import time
import typing
from collections.abc import Iterator, Mapping, Sequence
from dataclasses import dataclass, field
from enum import IntEnum, StrEnum
from pathlib import Path
from typing import Any, Literal

import cyclopts

from . import __version__
from .cost import (
    Duration,
    EstimatedUSD,
    Unpriced,
    compare_residency,
    estimate_run,
    run_report,
)
from .errors import AgentdError, AuthTokenMintError, HttpError, Unauthorized

# Safe at module level despite the lazy `from .sandbox import Sandbox` inside the
# handlers below: `sandbox` imports boto3 lazily too, so naming a constant from it
# costs nothing and does not drag the SDK in.
from .sandbox import MICROVM_REGIONS as _MICROVM_REGIONS
from .sizing import size_class_for

#: The envelope's own version, bumped when a field's meaning changes rather than
#: when a command is added. An agent that pinned to "1" must keep parsing.
API_VERSION = "1"

#: Re-exported from `sandbox` rather than restated. The two lists had already
#: diverged: this one carried `eu-central-1`, which measurement on 2026-08-07 shows
#: does *not* carry MicroVMs — it was one of the three regions that answered
#: `AccessDeniedException` with a null message. A second copy of a list whose
#: correctness condition is completeness is a second copy that goes stale alone.
MICROVM_REGIONS = _MICROVM_REGIONS

#: MicroVMs are ARM64-only, so a daemon binary built for the host is the single
#: most common first-attempt failure — and it surfaces as a run-hook timeout,
#: which says nothing about architecture. `doctor` reads the ELF header.
REQUIRED_ELF_MACHINE = 0xB7  # EM_AARCH64

#: Where the CLI records what it created. A `run` that is killed mid-launch leaves
#: identifiers behind that only this file remembers, and an image wedged in
#: CREATING cannot be deleted later at all — so the identifiers are the only thing
#: standing between the operator and a leak they cannot even name.
STATE_DIR = Path(os.environ.get("MICROVM_STATE_DIR") or Path.home() / ".microvm" / "runs")


class Exit(IntEnum):
    """The exit-code contract. Append-only: a consumer branches on these.

    Split by *what the caller should do next*, which is the only distinction worth
    a separate integer. RETRYABLE means run it again unchanged. CREDENTIALS means
    fix an identity and no amount of waiting helps. The three platform codes are
    separate because each names a different trap with a different remedy, and
    collapsing them would send someone to re-read the wrong section of
    `docs/PLATFORM.md`.
    """

    OK = 0
    #: An exception no handler claimed. Deliberately distinct from every handled
    #: class: a bug in this CLI reported as a platform failure would send the
    #: reader to AWS.
    UNEXPECTED = 1
    INVALID_ARG = 2
    RETRYABLE = 3
    CREDENTIALS = 4
    PROTOCOL = 5
    BUILD_WEDGED = 6
    LAUNCH_DIED = 7
    WINDOW_CLOSED = 8
    PLATFORM = 9
    TIMEOUT = 10
    INTERRUPTED = 11
    PRECONDITION = 12
    #: The sandbox worked and the caller's command failed. Its own code because it
    #: is the one non-zero exit that means nothing is wrong with the platform, the
    #: credentials, or this CLI — a CI caller needs to tell "your tests failed"
    #: from "we never got a VM", and one shared code cannot say both.
    EXEC_FAILED = 13


class Code(StrEnum):
    """The machine-readable code in a failure envelope, one per `Exit`.

    A string beside the integer because the two are read by different consumers:
    a shell branches on `$?`, an agent parsing `--json` branches on `code` and
    should never have to keep an integer table.
    """

    ERR_UNEXPECTED = "ERR_UNEXPECTED"
    ERR_INVALID_ARG = "ERR_INVALID_ARG"
    ERR_RETRYABLE = "ERR_RETRYABLE"
    ERR_CREDENTIALS = "ERR_CREDENTIALS"
    ERR_PROTOCOL = "ERR_PROTOCOL"
    ERR_BUILD_WEDGED = "ERR_BUILD_WEDGED"
    ERR_LAUNCH_DIED = "ERR_LAUNCH_DIED"
    ERR_WINDOW_CLOSED = "ERR_WINDOW_CLOSED"
    ERR_PLATFORM = "ERR_PLATFORM"
    ERR_TIMEOUT = "ERR_TIMEOUT"
    ERR_INTERRUPTED = "ERR_INTERRUPTED"
    ERR_PRECONDITION = "ERR_PRECONDITION"
    ERR_EXEC_FAILED = "ERR_EXEC_FAILED"


@dataclass(frozen=True)
class ExitSpec:
    """One row of the exit-code table: the integer, the code, and the remedy."""

    exit: Exit
    code: Code | None
    meaning: str
    finding: str = ""


#: Ordered by exit code so the rendered table reads like the contract it is. The
#: `finding` column names the `docs/PLATFORM.md` section, because a caller who hits
#: one of the platform codes needs the measurement, not just the word.
EXIT_TABLE: tuple[ExitSpec, ...] = (
    ExitSpec(Exit.OK, None, "the command did what it said"),
    ExitSpec(
        Exit.UNEXPECTED,
        Code.ERR_UNEXPECTED,
        "an exception no handler claimed — a bug in this CLI, not the platform",
    ),
    ExitSpec(
        Exit.INVALID_ARG,
        Code.ERR_INVALID_ARG,
        "the request was refused locally, before any AWS call",
    ),
    ExitSpec(
        Exit.RETRYABLE,
        Code.ERR_RETRYABLE,
        "a transient condition; run the identical command again",
        "Endpoint authentication",
    ),
    ExitSpec(
        Exit.CREDENTIALS,
        Code.ERR_CREDENTIALS,
        "an identity is wrong or absent; waiting will not fix it",
    ),
    ExitSpec(
        Exit.PROTOCOL,
        Code.ERR_PROTOCOL,
        "the daemon rejected the request on its merits",
    ),
    ExitSpec(
        Exit.BUILD_WEDGED,
        Code.ERR_BUILD_WEDGED,
        "the image build was never scheduled — the clientToken replay signature",
        "`clientToken` is a permanent idempotency key",
    ),
    ExitSpec(
        Exit.LAUNCH_DIED,
        Code.ERR_LAUNCH_DIED,
        "the MicroVM reached a terminal state before RUNNING; read stateReason",
        "`runHookPayload` arrives wrapped, not as the body",
    ),
    ExitSpec(
        Exit.WINDOW_CLOSED,
        Code.ERR_WINDOW_CLOSED,
        "the launch-time suspended window passed, so there is nothing to resume",
        "`idlePolicy`",
    ),
    ExitSpec(
        Exit.PLATFORM,
        Code.ERR_PLATFORM,
        "a control-plane failure with no more specific class",
    ),
    ExitSpec(
        Exit.TIMEOUT,
        Code.ERR_TIMEOUT,
        "a client-side deadline elapsed; the VM and the exec are untouched",
    ),
    ExitSpec(
        Exit.INTERRUPTED,
        Code.ERR_INTERRUPTED,
        "interrupted after launch; teardown ran and any leak is named in the payload",
        "The build log group survives Terraform",
    ),
    ExitSpec(
        Exit.PRECONDITION,
        Code.ERR_PRECONDITION,
        "a prerequisite is missing — run `microvm doctor`",
    ),
    ExitSpec(
        Exit.EXEC_FAILED,
        Code.ERR_EXEC_FAILED,
        "the sandbox worked and the command in it exited non-zero",
    ),
)


class CliError(Exception):
    """A failure already classified into the contract.

    Carries the `docs/PLATFORM.md` finding when one applies, because the whole
    reason these codes are distinct is that each sends the reader to a different
    measurement. `payload` survives into the envelope so a partial result — the
    identifiers a teardown could not delete, most importantly — is still machine
    readable on the failure path.
    """

    def __init__(
        self,
        exit_code: Exit,
        message: str,
        *,
        finding: str = "",
        suggestions: Sequence[str] = (),
        payload: Mapping[str, Any] | None = None,
    ) -> None:
        super().__init__(message)
        self.exit_code = exit_code
        self.message = message
        self.finding = finding
        self.suggestions = list(suggestions)
        self.payload = dict(payload or {})

    @property
    def code(self) -> Code:
        spec = next(row for row in EXIT_TABLE if row.exit is self.exit_code)
        assert spec.code is not None
        return spec.code


class AlreadyReported(Exception):
    """A non-zero exit whose envelope has already been written.

    Exists for exactly one case: `run` succeeded at making a sandbox and the
    command inside it exited non-zero. The result — output, cost, identifiers — is
    a *success* envelope and the caller still needs a non-zero code. Raising a
    `CliError` there would print a second envelope to stdout, which breaks the
    one-envelope-per-invocation rule that AC-5-2's guard checks by parsing stdout
    as a single JSON document.
    """

    def __init__(self, exit_code: Exit) -> None:
        super().__init__(f"already reported, exiting {int(exit_code)}")
        self.exit_code = exit_code


#: Signatures the library embeds in its own error messages, mapped to the code
#: each one earns. This table exists because `sandbox.py` raises `RuntimeError` for
#: all three traps, and the CLI's contract is that they are *different* failures
#: with different remedies. Matching on the library's message is a seam, not a
#: preference — the alternative is a distinct exception type per trap, which lives
#: in the library and is the right eventual shape. Anchored on the literal the
#: library also prints to the operator, so a reworded message fails the CLI's own
#: table-driven test rather than silently collapsing two codes into one.
_TRAP_SIGNATURES: tuple[tuple[str, Exit, str], ...] = (
    (
        "clientToken replay signature",
        Exit.BUILD_WEDGED,
        "`clientToken` is a permanent idempotency key",
    ),
    (
        "before RUNNING",
        Exit.LAUNCH_DIED,
        "`runHookPayload` arrives wrapped, not as the body",
    ),
    ("suspendedDurationSeconds", Exit.WINDOW_CLOSED, "`idlePolicy`"),
    ("will never reach", Exit.WINDOW_CLOSED, "`idlePolicy`"),
)


def classify(exc: BaseException) -> CliError:
    """Maps any exception onto exactly one row of the exit-code table.

    Order is the contract. `Unauthorized` is checked before the generic retryable
    test because it is an `HttpError` whose remedy is a credential rather than a
    wait, and `AgentdError.retryable` is checked before the status split because
    the library already made that judgement and the CLI has no business
    second-guessing it — retrying a 401 forever and failing a launch 200 ms from
    ready are the two mistakes this ordering prevents.
    """
    if isinstance(exc, CliError):
        return exc
    if isinstance(exc, KeyboardInterrupt):
        return CliError(Exit.INTERRUPTED, "interrupted")
    if isinstance(exc, cyclopts.exceptions.CycloptsError):
        # An unknown command, a misspelled option, a missing argument, a value that
        # will not coerce. All argument errors, and all fixable without touching AWS.
        # cyclopts' own message already carries its did-you-mean suggestion.
        return CliError(
            Exit.INVALID_ARG,
            str(exc),
            suggestions=["`microvm manifest` lists every command and its options"],
        )
    if isinstance(exc, Unauthorized):
        return CliError(
            Exit.CREDENTIALS,
            str(exc),
            suggestions=["the agent token does not match the one the run hook installed"],
        )
    if isinstance(exc, AuthTokenMintError):
        return CliError(
            Exit.RETRYABLE,
            str(exc),
            finding="Endpoint authentication",
            suggestions=["minting is inside the request path; the identical command may succeed"],
        )
    if isinstance(exc, AgentdError):
        if exc.retryable:
            return CliError(Exit.RETRYABLE, str(exc))
        if isinstance(exc, HttpError):
            return CliError(Exit.PROTOCOL, str(exc))
        return CliError(Exit.PLATFORM, str(exc))
    if isinstance(exc, TimeoutError):
        return CliError(
            Exit.TIMEOUT,
            str(exc),
            suggestions=["polling is read-only, so the exec and its output are untouched"],
        )
    if isinstance(exc, ValueError):
        # Every local reject in the library is a `ValueError` carrying the finding
        # in its own text: an off-table size class, a WORKDIR nothing declares, a
        # FROM that contradicts the base image ARN. Reported as an argument error
        # because that is what it is — the caller can fix it without touching AWS.
        return CliError(Exit.INVALID_ARG, str(exc))
    if isinstance(exc, RuntimeError):
        text = str(exc)
        for signature, code, finding in _TRAP_SIGNATURES:
            if signature in text:
                return CliError(code, text, finding=finding)
        return CliError(Exit.PLATFORM, text)
    if _is_credential_failure(exc):
        return CliError(
            Exit.CREDENTIALS,
            str(exc) or type(exc).__name__,
            suggestions=["run `microvm doctor` to see which credential the SDK could not resolve"],
        )
    return CliError(Exit.UNEXPECTED, f"{type(exc).__name__}: {exc}")


#: botocore's credential and region failures, by class name. Stable public surface
#: of the SDK, matched by name rather than by `isinstance` because importing
#: botocore here is what AC-5-4's static guard forbids — and it is right to: an
#: import is how a CLI grows a second path to AWS.
_CREDENTIAL_ERRORS = frozenset(
    {
        "NoCredentialsError",
        "PartialCredentialsError",
        "NoRegionError",
        "CredentialRetrievalError",
        "UnauthorizedSSOTokenError",
        "TokenRetrievalError",
        "ProfileNotFound",
    }
)


def _is_credential_failure(exc: BaseException) -> bool:
    """True for botocore's credential and region errors, without importing botocore."""
    # The whole MRO, not just the concrete class: botocore raises several subclasses
    # of `PartialCredentialsError`, and matching only the leaf would let a new one
    # fall through to ERR_UNEXPECTED — reporting an expired SSO session as a bug in
    # this CLI.
    return bool(_CREDENTIAL_ERRORS & {cls.__name__ for cls in type(exc).__mro__})


# -- the envelope ------------------------------------------------------------


@dataclass
class Output:
    """Where each stream goes, and the rule that keeps them apart.

    Progress on stderr and exactly one envelope on stdout is not a style choice:
    AC-5-2's guard parses stdout as a single JSON document with progress enabled,
    so a single `print()` of a status line breaks the contract. Holding both
    streams in one object means every write in this module goes through a method
    that already knows which is which.
    """

    as_json: bool = False
    dense: bool = False
    quiet: bool = False
    stdout: typing.IO[str] = field(default_factory=lambda: sys.stdout)
    stderr: typing.IO[str] = field(default_factory=lambda: sys.stderr)

    def progress(self, message: str) -> None:
        """A human-facing line. Never stdout, whatever the format."""
        if not self.quiet:
            print(message, file=self.stderr, flush=True)

    def warn(self, message: str) -> None:
        """A warning the operator must see even with `--quiet`.

        Not suppressed by `--quiet`, because the two things that reach here are a
        stale rate table and a resource that leaked — and a leak nobody is told
        about is the failure `--quiet` should not be able to buy.
        """
        print(f"warning: {message}", file=self.stderr, flush=True)

    def emit(self, envelope: Mapping[str, Any], text: str) -> None:
        """The single write to stdout per invocation."""
        if self.as_json:
            separators = (",", ":") if self.dense else None
            print(
                jsonlib.dumps(envelope, separators=separators, indent=None if self.dense else 2),
                file=self.stdout,
                flush=True,
            )
        else:
            print(text, file=self.stdout, flush=True)


#: The shell convention for death by SIGINT, and what cyclopts turns a handler's
#: `KeyboardInterrupt` into. Named because `dispatch` has to recognize it: the
#: exception is gone by the time it reaches that frame.
_SIGINT_EXIT_CODE = 130

#: Set by `dispatch` for the duration of one invocation, so a handler and the error
#: path write to the same two streams. A module-level slot rather than a parameter
#: threaded through eleven handlers, because cyclopts binds handler arguments from
#: the command line and an extra one would appear in `--help` and in the manifest as
#: an option a caller could pass.
_ACTIVE_OUTPUT: Output | None = None


def make_output(*, as_json: bool, dense: bool, quiet: bool) -> Output:
    """The `Output` a handler renders through.

    Reuses the invocation's own object when there is one, so a test can inject
    streams and a failure reported from `dispatch` lands on the same pair a handler
    would have used. The flags are re-read from the handler's parsed arguments
    because `dispatch` only saw raw tokens.
    """
    if _ACTIVE_OUTPUT is not None:
        _ACTIVE_OUTPUT.as_json = as_json
        _ACTIVE_OUTPUT.dense = dense
        _ACTIVE_OUTPUT.quiet = quiet
        return _ACTIVE_OUTPUT
    return Output(as_json=as_json, dense=dense, quiet=quiet)


def ok_envelope(kind: str, data: Mapping[str, Any]) -> dict[str, Any]:
    """A success envelope. `type` is the discriminant an agent branches on first."""
    return {"status": "ok", "apiVersion": API_VERSION, "type": kind, "data": dict(data)}


def error_envelope(error: CliError) -> dict[str, Any]:
    """A failure envelope carrying the code, the exit code, and the finding.

    `finding` is in the envelope rather than only in the message because it is the
    field that turns a failure into a lookup: an agent that reads
    `finding: "`idlePolicy`"` can go read the measurement instead of guessing at a
    retry policy. Empty string when no measured finding applies, never absent —
    a key that appears conditionally is a key every consumer has to guard.
    """
    return {
        "status": "error",
        "apiVersion": API_VERSION,
        "error": error.message,
        "code": str(error.code),
        "exitCode": int(error.exit_code),
        "finding": error.finding,
        "suggestions": list(error.suggestions),
        "data": dict(error.payload),
    }


def render_error(error: CliError) -> str:
    lines = [f"error {error.code}: {error.message}"]
    if error.finding:
        lines.append(f"  see docs/PLATFORM.md, '{error.finding}'")
    lines += [f"  hint: {s}" for s in error.suggestions]
    for key, value in sorted(error.payload.items()):
        lines.append(f"  {key}: {value}")
    return "\n".join(lines)


# -- the run ledger ----------------------------------------------------------


@dataclass
class RunLedger:
    """What one invocation created, so an interrupt can name what it could not delete.

    On disk rather than in memory only: the identifiers are worthless to the
    operator if the process that held them is the process that died. An image
    wedged in `CREATING` cannot be deleted afterward at all, and a service-created
    log group outlives `terraform destroy`, so the identifier *is* the remedy.
    """

    run_id: str
    region: str
    image_identifier: str | None = None
    image_name: str | None = None
    microvm_id: str | None = None
    #: Identifiers teardown tried and failed to remove. The operator's to-do list.
    leaked: list[str] = field(default_factory=list)
    path: Path | None = None

    def record(self, **fields: Any) -> None:
        for key, value in fields.items():
            setattr(self, key, value)
        self.flush()

    def flush(self) -> None:
        if self.path is None:
            return
        with contextlib.suppress(OSError):
            self.path.parent.mkdir(parents=True, exist_ok=True)
            self.path.write_text(jsonlib.dumps(self.as_dict(), indent=2))

    def clear(self) -> None:
        """Removes the ledger once nothing is outstanding.

        Only when `leaked` is empty: a file left behind is how `microvm ls` knows
        there is something to tell the operator about, so clearing one that still
        names a live resource would hide exactly the case the file exists for.
        """
        if self.leaked or self.path is None:
            return
        with contextlib.suppress(OSError):
            self.path.unlink()

    def as_dict(self) -> dict[str, Any]:
        return {
            "runId": self.run_id,
            "region": self.region,
            "imageIdentifier": self.image_identifier,
            "imageName": self.image_name,
            "microvmId": self.microvm_id,
            "leaked": list(self.leaked),
        }


def new_ledger(region: str, *, state_dir: Path | None = None) -> RunLedger:
    """A ledger keyed by a per-invocation id, written under the state directory."""
    # Not `secrets` — this names a local file, not an idempotency token, and the
    # library is emphatic that token minting is its own job.
    run_id = f"{int(time.time())}-{os.getpid()}"
    root = state_dir or STATE_DIR
    return RunLedger(run_id=run_id, region=region, path=root / f"{run_id}.json")


# -- the library seam --------------------------------------------------------


@dataclass(frozen=True)
class Infra:
    """The three account-specific values every AWS command needs.

    Read from flags or from the environment, and *resolved before any call*: a
    missing bucket discovered halfway through a build is a build that has already
    uploaded nothing and created nothing, but has also already spent the caller's
    attention. `doctor` reports the same three.
    """

    region: str
    bucket: str | None = None
    build_role_arn: str | None = None
    execution_role_arn: str | None = None

    def require(self, *names: str) -> None:
        """Rejects a command that cannot possibly succeed, naming every gap at once.

        Every gap rather than the first, because these arrive together from one
        Terraform apply and reporting them one per attempt costs the caller three
        round trips to learn one fact.
        """
        missing = [name for name in names if getattr(self, name) in (None, "")]
        if not missing:
            return
        env = {
            "bucket": "MICROVM_BUCKET",
            "build_role_arn": "MICROVM_BUILD_ROLE_ARN",
            "execution_role_arn": "MICROVM_EXECUTION_ROLE_ARN",
        }
        flags = ", ".join(f"--{name.replace('_', '-')} (or ${env[name]})" for name in missing)
        raise CliError(
            Exit.PRECONDITION,
            f"missing required infrastructure: {flags}",
            suggestions=[
                "`terraform -chdir=conformance/infra output` prints all three",
                "`microvm doctor` checks them alongside credentials and the daemon binary",
            ],
        )


def resolve_infra(
    region: str | None,
    bucket: str | None,
    build_role_arn: str | None,
    execution_role_arn: str | None,
    env: Mapping[str, str] | None = None,
) -> Infra:
    """Flags win over environment, and the region has a documented default.

    `AWS_REGION` before `AWS_DEFAULT_REGION` because that is boto3's own order, and
    a CLI that resolved the region differently from the SDK it calls would produce a
    connector ARN for one region and a client for another — an ARN mismatch that
    reads as a malformed connector rather than as a region disagreement.
    """
    source = os.environ if env is None else env
    return Infra(
        region=(
            region or source.get("AWS_REGION") or source.get("AWS_DEFAULT_REGION") or "us-east-1"
        ),
        bucket=bucket or source.get("MICROVM_BUCKET"),
        build_role_arn=build_role_arn or source.get("MICROVM_BUILD_ROLE_ARN"),
        execution_role_arn=execution_role_arn or source.get("MICROVM_EXECUTION_ROLE_ARN"),
    )


#: The one factory the CLI uses to reach AWS, and the seam every test patches.
#: A single name rather than `Sandbox(...)` scattered across eleven handlers is what
#: makes AC-5-4's behavioral guard possible: patch this to raise and every command
#: that touches AWS must fail. A handler that constructed its own `Sandbox` would
#: still pass a "did it fail" check while bypassing the seam, so the guard also
#: asserts the failure *is* the patched one.
def open_sandbox(infra: Infra, *, port: int | None = None) -> Any:
    """Constructs the library's `Sandbox` for `infra`. Imported lazily.

    Lazy because `sandbox.py` is where boto3 lives, and `microvm manifest`,
    `microvm cost --estimate`, and `microvm doctor` must all work in an environment
    with no credentials at all — which is also what lets the test suite import this
    module without AWS.
    """
    from .sandbox import Sandbox

    kwargs: dict[str, Any] = {"region": infra.region}
    if port is not None:
        kwargs["port"] = port
    return Sandbox(**kwargs)


def attach_session(
    infra: Infra, *, endpoint: str, agent_token: str, microvm_id: str, port: int | None = None
) -> Any:
    """A `Session` for a VM this invocation did not launch.

    The second seam, and it is separate because it needs no image and no build
    role: `microvm exec` against an already-running VM is the common case in a
    loop, and routing it through `open_sandbox` would demand infrastructure the
    command does not use. Both seams are patched by AC-5-4's behavioral guard.
    """
    from .sandbox import SERVICE, _client
    from .session import Session

    kwargs: dict[str, Any] = {
        "endpoint": endpoint,
        "agent_token": agent_token,
        "microvm_id": microvm_id,
        "microvm_client": _client(SERVICE, infra.region),
    }
    if port is not None:
        kwargs["port"] = port
    return Session(**kwargs)


# -- cost rendering ----------------------------------------------------------


def report_to_dict(report: Any) -> dict[str, Any]:
    """A `CostReport` as JSON, keeping every label the report carries.

    `amount` is an object rather than a number, and that is the whole point: an
    `Unpriced` line must not serialize as `0.0`, because a consumer summing the
    column would produce an invoice that flatters us. So each line item says which
    of the two it is, and the total says whether it is a lower bound.
    """
    return {
        "label": report.label,
        "size": {
            "baselineMib": report.size.baseline_mib,
            "baselineVcpu": report.size.baseline_vcpu,
            "peakMib": report.size.peak_mib,
            "peakVcpu": report.size.peak_vcpu,
            "describe": report.size.describe(),
        },
        "rates": {
            "region": report.rates.region,
            "retrieved": report.rates.retrieved.isoformat(),
            "sourceUrl": report.rates.source_url,
        },
        # Not "cost": these are estimates derived from published rates, and the
        # only place the distinction can survive a copy-paste is the field name.
        "estimated": True,
        "fullyMeasured": report.fully_measured,
        "complete": report.complete,
        "staleness": report.staleness,
        "items": [_line_to_dict(item) for item in report.items],
        "total": {
            "priced": str(report.total.priced.amount),
            "isLowerBound": report.total.is_lower_bound,
            "render": str(report.total),
        },
    }


def _line_to_dict(item: Any) -> dict[str, Any]:
    amount: dict[str, Any]
    if isinstance(item.amount, EstimatedUSD):
        amount = {"kind": "estimated-usd", "usd": str(item.amount.amount)}
    else:
        assert isinstance(item.amount, Unpriced)
        # No `usd` key at all. A null would be summed as zero by anything
        # permissive, which is the one arithmetic this module refuses to enable.
        amount = {"kind": "unpriced", "reason": item.amount.reason}
    return {
        "phase": item.phase.value,
        "line": item.line.value if item.line is not None else None,
        "quantity": str(item.quantity),
        "unit": item.unit,
        "amount": amount,
        "duration": (
            None
            if item.duration is None
            else {"seconds": item.duration.seconds, "provenance": item.duration.provenance.value}
        ),
        "note": item.note,
    }


def measured_report(
    *,
    memory_mib: int,
    running_sec: float,
    build_sec: float | None,
    image_gb: float | None,
    label: str,
) -> Any:
    """The cost of a run this CLI just performed, with timed phases labelled measured.

    `Duration.measured` for the phases a clock ran and nothing at all for the ones
    it did not: the library refuses an unlabelled duration, so there is no way for
    a projected figure to enter here disguised as a timing.
    """
    return run_report(
        size=memory_mib,
        running=Duration.measured(running_sec) if running_sec else None,
        image_build=Duration.measured(build_sec) if build_sec else None,
        image_gb=image_gb,
        label=label,
    )


# -- run: the headline -------------------------------------------------------


@dataclass
class RunOutcome:
    """Everything `run` learned, so the handler only formats.

    A dataclass rather than a tuple because the fields are read by name in three
    renderers (human, JSON, dense) and a positional shape is how the third one
    silently prints the exit code where the duration belongs.
    """

    image_identifier: str | None = None
    image_name: str | None = None
    microvm_id: str | None = None
    endpoint: str | None = None
    agent_token: str | None = None
    exec_exit_code: int | None = None
    stdout: str = ""
    stderr: str = ""
    truncated: bool = False
    build_seconds: float = 0.0
    running_seconds: float = 0.0
    kept: bool = False
    leaked: list[str] = field(default_factory=list)
    cost: dict[str, Any] | None = None


@contextlib.contextmanager
def teardown_guard(
    box: Any, ledger: RunLedger, out: Output, *, keep: bool, delete_image: bool
) -> Iterator[None]:
    """Tears the VM down on the way out, however the block ends, and names what leaked.

    `keep` is opt-in and that asymmetry is deliberate: a CLI that leaves a billable
    VM running by default is worse than no CLI, because the bill arrives a month
    after the person forgot they ran it. `--keep` prints the identifiers precisely
    because the caller has just taken responsibility for them.

    Teardown runs even on `KeyboardInterrupt`, which is AC-5-6: a CLI is the surface
    most likely to be killed mid-run, and an image left in `CREATING` cannot be
    deleted afterward at all. So the identifiers are recorded before the delete is
    attempted, not after.
    """
    try:
        yield
    finally:
        _tear_down(box, ledger, out, keep=keep, delete_image=delete_image)


def _tear_down(box: Any, ledger: RunLedger, out: Output, *, keep: bool, delete_image: bool) -> None:
    """The body of `teardown_guard`'s `finally`, as a function.

    Extracted only so the early return for `--keep` is a `return` in a function
    rather than in a `finally` block — which Python allows and warns about, because
    it discards any in-flight exception. Discarding the caller's real failure is
    exactly what `Sandbox.terminate` goes out of its way not to do.
    """
    if keep:
        out.progress(f"keeping {ledger.microvm_id or 'the microvm'} — you own the bill now")
        if ledger.microvm_id:
            out.progress(f"  release it with: microvm terminate {ledger.microvm_id}")
        ledger.leaked = [x for x in (ledger.microvm_id, ledger.image_identifier) if x]
        ledger.flush()
        return
    # Recorded as leaked *first*, cleared only on a delete that reported success.
    # The other order — try, then record on failure — loses the identifier when the
    # process dies inside the call, which is exactly the interrupt case this exists
    # for.
    outstanding = [x for x in (ledger.microvm_id, ledger.image_identifier) if x]
    ledger.record(leaked=outstanding)
    out.progress("tearing down")
    deleted: list[str] = []
    # `terminate` handles the VM and the log group and reports nothing, because it
    # runs in a `finally` and never raises. The image goes through its own entry
    # point instead, which returns whether it worked — and that boolean is the only
    # honest way to know whether the identifier belongs on the operator's to-do
    # list. Inferring it from `terminate` not raising would report a wedged image as
    # cleaned up.
    # Order matters, and getting it wrong leaked a log group on the first live run
    # of this command. The service owns the build log group, so it can still write
    # to it while an image is deleting — deleting the group first leaves the service
    # free to recreate it, and the leak then survives a teardown that reported
    # success. So: terminate the VM, delete the image, and take the log group last.
    # `conformance/probe_suspend_resume.py` hit the identical ordering trap.
    with contextlib.suppress(Exception):
        box.terminate(delete_image=False, delete_log_group=False)
        if ledger.microvm_id:
            deleted.append(ledger.microvm_id)
    if delete_image and ledger.image_identifier:
        with contextlib.suppress(Exception):
            if box.delete_image():
                deleted.append(ledger.image_identifier)
    if delete_image:
        with contextlib.suppress(Exception):
            box.delete_build_log_group()
    ledger.leaked = [x for x in outstanding if x not in deleted]
    for identifier in ledger.leaked:
        out.warn(
            f"could not delete {identifier} — it is still billing. An image in CREATING "
            "cannot be deleted at all (docs/PLATFORM.md, '`clientToken` is a permanent "
            "idempotency key'), so record this id."
        )
    ledger.flush()
    ledger.clear()


def do_run(
    *,
    infra: Infra,
    binary: Path,
    name: str,
    command: str | None,
    memory_mib: int,
    dockerfile: Path | None,
    repair_identity: bool,
    egress: bool,
    keep: bool,
    exec_timeout: float,
    max_idle_sec: int,
    suspended_sec: int,
    max_duration_sec: int,
    port: int | None,
    out: Output,
    state_dir: Path | None = None,
) -> RunOutcome:
    """Build, launch, exec, report, tear down — the whole thing, once.

    Every step is a library call. The CLI's contribution is the order, the two
    clocks, and the guarantee that the VM is gone at the end whether or not the
    middle worked.
    """
    infra.require("bucket", "build_role_arn", "execution_role_arn")
    if not binary.exists():
        raise CliError(
            Exit.PRECONDITION,
            f"daemon binary not found: {binary}",
            suggestions=[
                "build it with `cargo build --release -p agentd"
                " --target aarch64-unknown-linux-musl`",
                "`microvm doctor --binary <path>` checks the architecture too",
            ],
        )

    ledger = new_ledger(infra.region, state_dir=state_dir)
    box = open_sandbox(infra, port=port)
    outcome = RunOutcome(image_name=name)

    with teardown_guard(box, ledger, out, keep=keep, delete_image=not keep):
        out.progress(f"building image {name} ({size_class_for(memory_mib).describe()})")
        build_started = time.monotonic()
        image = box.build_image(
            name=name,
            binary=binary,
            bucket=infra.bucket or "",
            build_role_arn=infra.build_role_arn or "",
            dockerfile=dockerfile.read_text() if dockerfile is not None else None,
            memory_mib=memory_mib,
            repair_guest_identity=repair_identity,
            # A label beside the library's own per-attempt nonce, never a token.
            # The library accepts no token at all, which is what makes the wedge
            # unwriteable rather than merely defaulted.
            token_scope=name,
        )
        outcome.build_seconds = time.monotonic() - build_started
        outcome.image_identifier = image.identifier
        ledger.record(image_identifier=image.identifier, image_name=name)
        out.progress(f"image {image.identifier} built in {outcome.build_seconds:.0f}s")

        out.progress("launching")
        run_started = time.monotonic()
        session = box.run(
            execution_role_arn=infra.execution_role_arn or "",
            egress=egress,
            max_idle_sec=max_idle_sec,
            suspended_sec=suspended_sec,
            max_duration_sec=max_duration_sec,
            token_scope=name,
        )
        outcome.microvm_id = box.microvm_id
        outcome.endpoint = session.endpoint
        outcome.agent_token = box.agent_token
        ledger.record(microvm_id=box.microvm_id)
        out.progress(f"microvm {box.microvm_id} RUNNING at {session.endpoint}")

        session.wait_until_ready()
        if command is not None:
            out.progress(f"exec: {command}")
            result = session.run_sync(command, shell=True, timeout=exec_timeout)
            outcome.exec_exit_code = result.exit_code
            outcome.stdout = result.stdout or ""
            outcome.stderr = result.stderr or ""
            outcome.truncated = result.truncated
        outcome.running_seconds = time.monotonic() - run_started

    outcome.kept = keep
    outcome.leaked = list(ledger.leaked)
    report = measured_report(
        memory_mib=memory_mib,
        running_sec=outcome.running_seconds,
        build_sec=outcome.build_seconds,
        # The image's own size is not observable from any API this client calls, so
        # the baseline footprint stands in and the note on the line item says so.
        # A storage line omitted entirely would make a create-and-destroy run look
        # like it cost only its compute, and the one-week minimum retention means
        # storage is in fact the floor.
        image_gb=size_class_for(memory_mib).baseline_gb,
        label=f"run {name}",
    )
    if report.staleness:
        out.warn(report.staleness)
    outcome.cost = report_to_dict(report)
    out.progress(report.render())
    return outcome


def render_run(outcome: RunOutcome, *, dense: bool) -> str:
    """The human view. Output first, because output is what the caller asked for."""
    if dense:
        # TSV an agent can `split()`, with the exit code first so a shell can read
        # field one without parsing anything.
        return "\n".join(
            [
                f"exit\t{outcome.exec_exit_code if outcome.exec_exit_code is not None else ''}",
                f"microvm\t{outcome.microvm_id or ''}",
                f"image\t{outcome.image_identifier or ''}",
                f"running_sec\t{outcome.running_seconds:.1f}",
                f"leaked\t{','.join(outcome.leaked)}",
            ]
        )
    lines: list[str] = []
    if outcome.stdout:
        lines.append(outcome.stdout.rstrip("\n"))
    if outcome.stderr:
        lines.append(outcome.stderr.rstrip("\n"))
    if outcome.exec_exit_code is not None:
        lines.append(f"exit code: {outcome.exec_exit_code}")
    if outcome.truncated:
        lines.append("note: output was truncated at the daemon's cap")
    if outcome.kept:
        lines.append(f"kept: microvm {outcome.microvm_id}, image {outcome.image_identifier}")
    for identifier in outcome.leaked:
        lines.append(f"LEAKED (still billing): {identifier}")
    if outcome.cost:
        lines.append(f"cost: {outcome.cost['total']['render']}")
    return "\n".join(lines) if lines else "done"


# -- doctor ------------------------------------------------------------------


@dataclass(frozen=True)
class Check:
    """One prerequisite, its verdict, and what to do about it.

    `ok=False` with `fatal=False` is a warning — a region we have not seen listed,
    a Terraform stack that may live elsewhere. The distinction matters because the
    exit code is derived from the fatal ones only, and a CLI that failed `doctor`
    over an advisory would train people to ignore it.
    """

    name: str
    ok: bool
    detail: str
    fatal: bool = True
    remedy: str = ""

    def as_dict(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "ok": self.ok,
            "detail": self.detail,
            "fatal": self.fatal,
            "remedy": self.remedy,
        }


def elf_machine(path: Path) -> int | None:
    """The `e_machine` field of an ELF header, or None if this is not an ELF file.

    Read directly rather than shelled out to `file(1)`, which is not installed
    everywhere and whose output format is prose. Sixteen bytes of header is the
    whole check: MicroVMs are ARM64-only, and a daemon built for the host produces
    an image whose CMD cannot exec — surfacing as a run-hook timeout that says
    nothing about architecture, one 45-minute build cycle later.
    """
    try:
        with path.open("rb") as handle:
            header = handle.read(20)
    except OSError:
        return None
    if len(header) < 20 or header[:4] != b"\x7fELF":
        return None
    # Byte 5 is EI_DATA: 1 little-endian, 2 big. e_machine is a 2-byte field at 18.
    endian = "little" if header[5] == 1 else "big"
    return int.from_bytes(header[18:20], endian)


def check_credentials(infra: Infra) -> Check:
    """Whether the SDK can resolve an identity at all, without spending a call.

    `get_caller_identity` is the cheapest question that proves credentials resolve
    *and* are accepted, which a local file read cannot. It is also the one place
    `doctor` deliberately reaches for a boto3 client that is not a MicroVMs client
    — routed through the library's own lazy factory so there is still exactly one
    import site for the SDK in this package.
    """
    try:
        from .sandbox import _client

        identity = _client("sts", infra.region).get_caller_identity()
    except Exception as exc:  # noqa: BLE001 - every failure here is a finding to report
        return Check(
            name="credentials",
            ok=False,
            detail=f"{type(exc).__name__}: {exc}",
            remedy="`aws sso login`, or set AWS_PROFILE / AWS_ACCESS_KEY_ID",
        )
    return Check(
        name="credentials",
        ok=True,
        detail=f"account {identity.get('Account')} as {identity.get('Arn', '')}",
    )


def check_region(infra: Infra) -> Check:
    listed = infra.region in MICROVM_REGIONS
    return Check(
        name="region",
        ok=listed,
        detail=(
            f"{infra.region} is a known MicroVMs region"
            if listed
            else f"{infra.region} is not in this client's list of MicroVMs regions"
        ),
        # Advisory: AWS adds regions faster than this constant is re-read, and a
        # hard failure here would block a caller who is right and we are stale.
        fatal=False,
        remedy="" if listed else f"known: {', '.join(sorted(MICROVM_REGIONS))}",
    )


def check_binary(binary: Path | None) -> Check:
    if binary is None:
        return Check(
            name="daemon-binary",
            ok=False,
            detail="no --binary given, so the architecture could not be checked",
            fatal=False,
            remedy="pass --binary target/aarch64-unknown-linux-musl/release/agentd",
        )
    if not binary.exists():
        return Check(
            name="daemon-binary",
            ok=False,
            detail=f"{binary} does not exist",
            remedy="cargo build --release -p agentd --target aarch64-unknown-linux-musl",
        )
    machine = elf_machine(binary)
    if machine is None:
        return Check(
            name="daemon-binary",
            ok=False,
            detail=f"{binary} is not an ELF binary",
            remedy="the image CMD must be a static aarch64 ELF, not a script or a wrapper",
        )
    if machine != REQUIRED_ELF_MACHINE:
        return Check(
            name="daemon-binary",
            ok=False,
            detail=(
                f"{binary} is ELF machine 0x{machine:x}, not aarch64 (0x{REQUIRED_ELF_MACHINE:x})"
            ),
            remedy=(
                "MicroVMs are ARM64-only. Rebuild for aarch64-unknown-linux-musl — a host-arch "
                "binary fails as a run-hook timeout, which says nothing about architecture."
            ),
        )
    return Check(name="daemon-binary", ok=True, detail=f"{binary} is aarch64 ELF")


def check_infra(infra: Infra) -> list[Check]:
    """The three Terraform outputs, each reported by name.

    Separate checks rather than one, because "infrastructure missing" sends someone
    to re-read a whole stack while "no execution role" sends them to one output.
    """
    fields = (
        ("bucket", infra.bucket, "MICROVM_BUCKET", "s3_bucket"),
        ("build-role", infra.build_role_arn, "MICROVM_BUILD_ROLE_ARN", "build_role_arn"),
        (
            "execution-role",
            infra.execution_role_arn,
            "MICROVM_EXECUTION_ROLE_ARN",
            "execution_role_arn",
        ),
    )
    return [
        Check(
            name=name,
            ok=bool(value),
            detail=value or f"unset (${env})",
            remedy=(
                ""
                if value
                else (
                    f"terraform -chdir=conformance/infra output -raw {output}"
                    " — or `mise run live:infra`"
                )
            ),
        )
        for name, value, env, output in fields
    ]


def check_terraform_stack(infra_dir: Path) -> Check:
    """Whether the conformance stack is applied, asked of Terraform rather than of a file.

    A `terraform.tfstate` on disk is not the same as a stack that exists: a
    destroyed stack leaves the file behind with an empty resource list, which is
    precisely the state that produces "bucket does not exist" three minutes into a
    build. `terraform output` answers the real question and needs no credentials.
    """
    binary = shutil.which("terraform")
    if binary is None:
        return Check(
            name="terraform-stack",
            ok=False,
            detail="terraform is not on PATH, so the stack state is unknown",
            fatal=False,
            remedy="mise install, or pass --bucket/--build-role-arn/--execution-role-arn directly",
        )
    if not infra_dir.exists():
        return Check(
            name="terraform-stack",
            ok=False,
            detail=f"{infra_dir} does not exist",
            fatal=False,
            remedy="the stack may live elsewhere; pass the three values as flags",
        )
    try:
        proc = subprocess.run(
            [binary, f"-chdir={infra_dir}", "output", "-json"],
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        outputs = jsonlib.loads(proc.stdout or "{}") if proc.returncode == 0 else {}
    except (OSError, ValueError, subprocess.SubprocessError) as exc:
        return Check(
            name="terraform-stack",
            ok=False,
            detail=f"could not read terraform output: {type(exc).__name__}: {exc}",
            fatal=False,
            remedy="terraform -chdir=conformance/infra init",
        )
    wanted = {"s3_bucket", "build_role_arn", "execution_role_arn"}
    present = wanted & set(outputs)
    if present == wanted:
        return Check(
            name="terraform-stack",
            ok=True,
            detail=f"applied: {outputs['s3_bucket'].get('value')}",
        )
    return Check(
        name="terraform-stack",
        ok=False,
        detail=f"stack is not applied (missing outputs: {', '.join(sorted(wanted - present))})",
        fatal=False,
        remedy="mise run live:infra",
    )


def run_doctor(infra: Infra, *, binary: Path | None, infra_dir: Path) -> list[Check]:
    """Every prerequisite, in the order they bite.

    Credentials first because nothing else can be checked without them, then the
    region the connector ARN is interpolated into, then the three account values,
    then the binary — which is last only because it is the one failure that costs a
    full build cycle rather than a call.
    """
    checks = [check_credentials(infra), check_region(infra)]
    checks += check_infra(infra)
    checks.append(check_terraform_stack(infra_dir))
    checks.append(check_binary(binary))
    return checks


def render_doctor(checks: Sequence[Check], *, dense: bool) -> str:
    if dense:
        return "\n".join(f"{c.name}\t{'ok' if c.ok else 'fail'}\t{c.detail}" for c in checks)
    lines = []
    for check in checks:
        mark = "PASS" if check.ok else ("FAIL" if check.fatal else "WARN")
        lines.append(f"{mark}  {check.name}: {check.detail}")
        if not check.ok and check.remedy:
            lines.append(f"      -> {check.remedy}")
    return "\n".join(lines)


# -- manifest ----------------------------------------------------------------

#: cyclopts registers these alongside real commands. Filtered by name rather than
#: by a hand-kept command list, so a new command appears in the manifest without
#: anyone remembering to add it — which is AC-5-3's requirement that the manifest be
#: derived and not maintained.
_META_COMMANDS = frozenset({"--help", "-h", "--version"})

#: The `type` discriminant each command puts in its success envelope, and the keys
#: that payload carries. Declared beside the commands rather than generated from a
#: return type because the handlers return None — they render. The manifest test
#: cross-checks this mapping against the registered command tree, so a command
#: added without an entry fails rather than shipping undescribed.
RESPONSE_TYPES: dict[str, tuple[str, tuple[str, ...]]] = {
    "run": (
        "microvm.run",
        (
            "imageIdentifier",
            "imageName",
            "microvmId",
            "endpoint",
            "execExitCode",
            "stdout",
            "stderr",
            "truncated",
            "buildSeconds",
            "runningSeconds",
            "kept",
            "leaked",
            "cost",
        ),
    ),
    "build": ("microvm.image", ("imageIdentifier", "imageName", "buildLogGroup", "size")),
    "exec": ("microvm.exec", ("execId", "exitCode", "stdout", "stderr", "truncated")),
    "suspend": ("microvm.state", ("microvmId", "state")),
    "resume": ("microvm.state", ("microvmId", "state", "endpoint")),
    "terminate": ("microvm.teardown", ("microvmId", "imageIdentifier", "leaked")),
    "ls": ("microvm.runs", ("runs",)),
    "logs": ("microvm.logs", ("logGroup", "lines")),
    "cost": ("microvm.cost", ("report", "comparison")),
    "doctor": ("microvm.doctor", ("checks", "ok")),
    "manifest": (
        "microvm.manifest",
        ("apiVersion", "cli", "version", "commands", "exitCodes", "envelope", "conventions"),
    ),
}


def _choices(argument: Any) -> list[str] | None:
    """The closed set an option accepts, or None when it is free-form.

    This is the field AC-5-5 is checked against: an option whose library
    counterpart is S1 — the size class, the capability intent, the connector intent
    — must report a closed set here, because the CLI is where an S1 guard is most
    easily downgraded to S3 by a convenience string flag.
    """
    choices = argument.get_choices()
    return [str(c) for c in choices] if choices else None


def build_manifest() -> dict[str, Any]:
    """The whole surface, derived from the registered command tree.

    Derived rather than written: a hand-maintained copy drifts the first time
    someone adds a flag, and the manifest's entire value to an agent is that it
    cannot be wrong. The exit-code table and the envelope shape come from the same
    module constants the runtime uses.
    """
    commands: list[dict[str, Any]] = []
    for name in app:
        if name in _META_COMMANDS:
            continue
        sub = app[name]
        handler = sub.default_command
        summary = (inspect.getdoc(handler) or "").split("\n\n")[0].strip() if handler else ""
        parameters = [
            {
                "name": argument.name,
                "type": getattr(argument.hint, "__name__", str(argument.hint)),
                "choices": _choices(argument),
                "required": bool(argument.required),
                "help": argument.parameter.help or "",
            }
            for argument in sub.assemble_argument_collection(parse_docstring=True)
        ]
        kind, keys = RESPONSE_TYPES.get(name, ("", ()))
        commands.append(
            {
                "name": name,
                "summary": summary,
                "parameters": parameters,
                "supportsJson": True,
                "responseType": kind,
                "responseKeys": list(keys),
            }
        )
    return {
        "apiVersion": API_VERSION,
        "cli": "microvm",
        "version": __version__,
        "commands": commands,
        "exitCodes": [
            {
                "exit": int(row.exit),
                "code": str(row.code) if row.code else None,
                "meaning": row.meaning,
                "finding": row.finding,
            }
            for row in EXIT_TABLE
        ],
        "envelope": {
            "discriminator": "status",
            "ok": {
                "status": "ok",
                "apiVersion": "string",
                "type": "string — one of responseType above",
                "data": "object — keys per responseKeys",
            },
            "error": {
                "status": "error",
                "apiVersion": "string",
                "error": "string — human readable, may be reworded between releases",
                "code": "string — stable, branch on this",
                "exitCode": "integer — matches the process exit code",
                "finding": "string — the docs/PLATFORM.md section, or empty",
                "suggestions": "array of string",
                "data": "object — partial results, e.g. leaked identifiers",
            },
        },
        "conventions": [
            "exactly one envelope object on stdout per invocation; progress is on stderr",
            "branch on `code`, never on `error`",
            "dollar figures are estimates derived from published rates, never an invoice",
            "an unpriced line item omits `usd` rather than reporting zero",
        ],
    }


def render_manifest(manifest: Mapping[str, Any], *, dense: bool) -> str:
    """The human view of the manifest: the two tables an operator actually reads."""
    if dense:
        return "\n".join(
            f"{c['name']}\t{c['responseType']}\t{','.join(p['name'] for p in c['parameters'])}"
            for c in manifest["commands"]
        )
    lines = [f"microvm {manifest['version']} — {len(manifest['commands'])} commands", ""]
    for command in manifest["commands"]:
        lines.append(f"  {command['name']:<10} {command['summary']}")
    lines += ["", "exit codes:"]
    for row in manifest["exitCodes"]:
        code = row["code"] or "-"
        lines.append(f"  {row['exit']:<3} {code:<20} {row['meaning']}")
    return "\n".join(lines)


# -- the command surface -----------------------------------------------------

app = cyclopts.App(
    name="microvm",
    version=__version__,
    help="Build, run, and tear down AWS Lambda MicroVMs. A thin layer over microvms_agentd.",
)

#: The five documented size-class baselines, as a closed set the parser enforces.
#: A `Literal` rather than an `int` because `sizing.size_class_for` rejects anything
#: off-table, and an option that accepts a value the library is known to refuse is an
#: S1 guard downgraded to a runtime error — which is precisely the CLI-shaped
#: regression AC-5-5 exists to catch.
#:
#: Spelled out rather than computed from `SIZE_CLASSES`, because a `Literal` built at
#: runtime is invisible to a type checker and the static half of the guarantee is the
#: half worth having. The cost of writing it twice is that the two can disagree, so a
#: test asserts the manifest's reported domain equals the size table — a new row that
#: does not reach this line fails there.
MemoryMib = Literal[512, 1024, 2048, 4096, 8192]


@app.command
def run(
    binary: Path,
    /,
    *,
    exec: str | None = None,
    name: str | None = None,
    memory: MemoryMib = 2048,
    dockerfile: Path | None = None,
    repair_identity: bool = False,
    egress: bool = False,
    keep: bool = False,
    timeout: float = 300.0,
    max_idle_sec: int = 600,
    suspended_sec: int = 600,
    max_duration_sec: int = 3600,
    port: int | None = None,
    region: str | None = None,
    bucket: str | None = None,
    build_role_arn: str | None = None,
    execution_role_arn: str | None = None,
    json: bool = False,
    dense: bool = False,
    quiet: bool = False,
) -> None:
    """Build an image, launch a VM, run a command, report the cost, tear it down.

    The whole eleven-call sequence as one command. Tears down by default: a CLI that
    leaves a billable VM running because someone closed a laptop is worse than no
    CLI. `--keep` opts out and prints the identifiers you have just taken
    responsibility for.

    Parameters
    ----------
    binary
        The aarch64 agentd binary to bake in as the image CMD.
    exec
        A shell command to run in the VM. Omitted launches and tears down, which is
        how you check that an image boots at all.
    name
        Image name. Defaults to a per-invocation name, because reusing one is how a
        clientToken replay wedges an image.
    memory
        Baseline MiB, which selects a documented size class. Defaults to the
        platform's own 2 GB rather than the cheapest class: baseline is also the
        floor of the burst range, and 0.5 GB OOM-kills a real test suite to save
        about three cents an hour.
    dockerfile
        A Dockerfile to use instead of the library's default. Its FROM must match
        the base image.
    repair_identity
        Widen the guest so `sethostname` and the `boot_id` bind mount work. Root in
        the guest is not enough for either.
    egress
        Give the VM outbound network. Omitted by default — a daemon needs none.
    keep
        Leave the VM and image running. You are then paying for them.
    timeout
        How long to wait for the exec, in seconds.
    max_idle_sec
        Suspend the VM after this much inbound-traffic idleness.
    suspended_sec
        Terminate the VM after this long suspended. A resume past it cannot work.
    max_duration_sec
        Hard ceiling on the VM's life.
    port
        The daemon's port inside the guest.
    region
        AWS region. Defaults to AWS_REGION, then us-east-1.
    bucket
        S3 bucket for the build artifact. Defaults to $MICROVM_BUCKET.
    build_role_arn
        Build role. Defaults to $MICROVM_BUILD_ROLE_ARN.
    execution_role_arn
        Execution role. Defaults to $MICROVM_EXECUTION_ROLE_ARN.
    json
        Emit the typed JSON envelope on stdout instead of human output.
    dense
        Token-lean output, for a consumer paying per token.
    quiet
        Suppress progress on stderr. Warnings still print.
    """
    out = make_output(as_json=json, dense=dense, quiet=quiet)
    infra = resolve_infra(region, bucket, build_role_arn, execution_role_arn)
    image_name = name or f"microvm-cli-{int(time.time())}"
    outcome = do_run(
        infra=infra,
        binary=binary,
        name=image_name,
        command=exec,
        memory_mib=int(memory),
        dockerfile=dockerfile,
        repair_identity=repair_identity,
        egress=egress,
        keep=keep,
        exec_timeout=timeout,
        max_idle_sec=max_idle_sec,
        suspended_sec=suspended_sec,
        max_duration_sec=max_duration_sec,
        port=port,
        out=out,
    )
    kind, _ = RESPONSE_TYPES["run"]
    out.emit(
        ok_envelope(
            kind,
            {
                "imageIdentifier": outcome.image_identifier,
                "imageName": outcome.image_name,
                "microvmId": outcome.microvm_id,
                "endpoint": outcome.endpoint,
                "execExitCode": outcome.exec_exit_code,
                "stdout": outcome.stdout,
                "stderr": outcome.stderr,
                "truncated": outcome.truncated,
                "buildSeconds": outcome.build_seconds,
                "runningSeconds": outcome.running_seconds,
                "kept": outcome.kept,
                "leaked": outcome.leaked,
                "cost": outcome.cost,
            },
        ),
        render_run(outcome, dense=dense),
    )
    # A failing workload gets a non-zero code but keeps its success envelope: the
    # sandbox did its job, and the output and cost the caller asked for are in
    # `data`. Mapped onto one stable code rather than passed through raw, because a
    # workload exiting 4 must not be indistinguishable from a credential failure.
    if outcome.exec_exit_code:
        raise AlreadyReported(Exit.EXEC_FAILED)


@app.command
def build(
    binary: Path,
    /,
    *,
    name: str | None = None,
    memory: MemoryMib = 2048,
    dockerfile: Path | None = None,
    repair_identity: bool = False,
    region: str | None = None,
    bucket: str | None = None,
    build_role_arn: str | None = None,
    port: int | None = None,
    json: bool = False,
    dense: bool = False,
    quiet: bool = False,
) -> None:
    """Build a MicroVM image and wait for it to be usable.

    Separate from `run` for the case where one image serves many launches, which is
    the shape that matters once a build is 45 minutes. Nothing is torn down here:
    an image is the durable artifact, and its one-week minimum snapshot retention
    means deleting it early saves nothing anyway.

    Parameters
    ----------
    binary
        The aarch64 agentd binary to bake in as the image CMD.
    name
        Image name. Defaults to a per-invocation name.
    memory
        Baseline MiB, selecting a documented size class.
    dockerfile
        A Dockerfile to use instead of the library's default.
    repair_identity
        Widen the guest so `sethostname` and the `boot_id` bind mount work.
    region
        AWS region. Defaults to AWS_REGION, then us-east-1.
    bucket
        S3 bucket for the build artifact. Defaults to $MICROVM_BUCKET.
    build_role_arn
        Build role. Defaults to $MICROVM_BUILD_ROLE_ARN.
    port
        The daemon's port inside the guest.
    json
        Emit the typed JSON envelope on stdout instead of human output.
    dense
        Token-lean output.
    quiet
        Suppress progress on stderr.
    """
    out = make_output(as_json=json, dense=dense, quiet=quiet)
    infra = resolve_infra(region, bucket, build_role_arn, None)
    infra.require("bucket", "build_role_arn")
    image_name = name or f"microvm-cli-{int(time.time())}"
    box = open_sandbox(infra, port=port)
    out.progress(f"building image {image_name} ({size_class_for(int(memory)).describe()})")
    image = box.build_image(
        name=image_name,
        binary=binary,
        bucket=infra.bucket or "",
        build_role_arn=infra.build_role_arn or "",
        dockerfile=dockerfile.read_text() if dockerfile is not None else None,
        memory_mib=int(memory),
        repair_guest_identity=repair_identity,
        token_scope=image_name,
    )
    kind, _ = RESPONSE_TYPES["build"]
    data = {
        "imageIdentifier": image.identifier,
        "imageName": image.name,
        # Named in the payload because the service creates it, Terraform never owns
        # it, and `terraform destroy` leaves it behind — so the caller who built
        # this image is the only one who will ever know to delete it.
        "buildLogGroup": image.build_log_group,
        "size": image.size.describe(),
    }
    out.emit(
        ok_envelope(kind, data),
        (
            f"{image.identifier}\t{image.name}\t{image.build_log_group}"
            if dense
            else "\n".join(
                [
                    f"image: {image.identifier}",
                    f"name: {image.name}",
                    f"size: {image.size.describe()}",
                    f"build log group: {image.build_log_group}",
                    "note: the service created that log group; "
                    "terraform destroy will not remove it",
                ]
            )
        ),
    )


@app.command
def exec(
    command: str,
    /,
    *,
    endpoint: str,
    agent_token: str,
    microvm_id: str,
    timeout: float = 300.0,
    cwd: str | None = None,
    port: int | None = None,
    region: str | None = None,
    json: bool = False,
    dense: bool = False,
    quiet: bool = False,
) -> None:
    """Run one command in a MicroVM that is already running.

    The loop shape: launch once with `run --keep`, then exec against it as many
    times as you like. Needs the three identifiers `run --keep` printed, because a
    `Session` holds no server-side state — every exec record and the bootstrap token
    live in the VM, so reattaching is just naming it.

    Parameters
    ----------
    command
        A shell command to run in the VM.
    endpoint
        The VM's endpoint, as reported by `run`.
    agent_token
        The agent token delivered to the VM at launch.
    microvm_id
        The MicroVM id, needed to mint the endpoint proxy token.
    timeout
        How long to wait for the command, in seconds.
    cwd
        Working directory. Omitted inherits the image WORKDIR, which is not the
        same as passing `/` — most public ARM64 bases declare none.
    port
        The daemon's port inside the guest.
    region
        AWS region, for minting the proxy token.
    json
        Emit the typed JSON envelope on stdout instead of human output.
    dense
        Token-lean output.
    quiet
        Suppress progress on stderr.
    """
    out = make_output(as_json=json, dense=dense, quiet=quiet)
    infra = resolve_infra(region, None, None, None)
    session = attach_session(
        infra,
        endpoint=endpoint,
        agent_token=agent_token,
        microvm_id=microvm_id,
        port=port,
    )
    out.progress(f"exec: {command}")
    result = session.run_sync(command, shell=True, timeout=timeout, cwd=cwd)
    kind, _ = RESPONSE_TYPES["exec"]
    data = {
        "execId": result.exec_id,
        "exitCode": result.exit_code,
        "stdout": result.stdout or "",
        "stderr": result.stderr or "",
        "truncated": result.truncated,
    }
    if dense:
        text = f"exit\t{result.exit_code}\n{result.stdout or ''}"
    else:
        parts = [p for p in (result.stdout, result.stderr) if p]
        text = "\n".join([*(p.rstrip("\n") for p in parts), f"exit code: {result.exit_code}"])
    out.emit(ok_envelope(kind, data), text)
    if result.exit_code:
        raise AlreadyReported(Exit.EXEC_FAILED)


def _attached_box(infra: Infra, microvm_id: str, *, port: int | None) -> Any:
    """A `Sandbox` bound to a VM this invocation did not launch.

    Suspend, resume, and terminate all need one, and all three set the fields the
    library would otherwise have set at launch. `_suspended_window_sec` is
    deliberately *not* set: the library's window check is skipped when the window is
    unknown, and inventing a default here would either reject an open window or
    accept a closed one — both worse than letting the service answer, which is what
    the terminal-state branch on the resume path does.
    """
    box = open_sandbox(infra, port=port)
    box.microvm_id = microvm_id
    return box


@app.command
def suspend(
    microvm_id: str,
    /,
    *,
    timeout: float = 300.0,
    port: int | None = None,
    region: str | None = None,
    json: bool = False,
    dense: bool = False,
    quiet: bool = False,
) -> None:
    """Freeze a MicroVM. It keeps its memory, filesystem, token, and endpoint.

    A freeze and restore, not a stop and start — measured, not assumed. A suspended
    2 GB VM pays snapshot storage of about $0.16 a month against roughly $100 a
    month running, which is what makes a warm pool viable. `microvm cost --compare`
    prints the break-even hold, because each cycle also pays a write plus a read.

    Parameters
    ----------
    microvm_id
        The MicroVM to freeze.
    timeout
        How long to wait for the state transition, in seconds.
    port
        The daemon's port inside the guest.
    region
        AWS region. Defaults to AWS_REGION, then us-east-1.
    json
        Emit the typed JSON envelope on stdout instead of human output.
    dense
        Token-lean output.
    quiet
        Suppress progress on stderr.
    """
    out = make_output(as_json=json, dense=dense, quiet=quiet)
    infra = resolve_infra(region, None, None, None)
    box = _attached_box(infra, microvm_id, port=port)
    out.progress(f"suspending {microvm_id}")
    state = box.suspend(timeout=timeout)
    kind, _ = RESPONSE_TYPES["suspend"]
    out.emit(
        ok_envelope(kind, {"microvmId": microvm_id, "state": state}),
        f"{microvm_id}\t{state}" if dense else f"{microvm_id} is {state}",
    )
    if state != "SUSPENDED":
        # A VM that died on the way into suspension is reported, not raised through,
        # because the library returns the state rather than throwing — suspend is
        # typically on a teardown path. But the caller asked for SUSPENDED and did
        # not get it, so the exit code has to say so.
        raise AlreadyReported(Exit.PLATFORM)


@app.command
def resume(
    microvm_id: str,
    /,
    *,
    endpoint: str,
    agent_token: str,
    timeout: float = 300.0,
    port: int | None = None,
    region: str | None = None,
    json: bool = False,
    dense: bool = False,
    quiet: bool = False,
) -> None:
    """Thaw a suspended MicroVM and report its endpoint.

    Fails fast rather than hanging when the launch-time `suspendedDurationSeconds`
    window has closed: the idle policy terminates a suspended VM once it passes, so
    "resume later" silently stops working and the VM is gone rather than slow.

    Parameters
    ----------
    microvm_id
        The MicroVM to thaw.
    endpoint
        Its endpoint from before the suspension. Measured to be unchanged across a
        cycle, but passed rather than assumed.
    agent_token
        The agent token from launch. Reused: the in-memory token survived, so no
        re-delivery and no re-bootstrap.
    timeout
        How long to wait for RUNNING, in seconds.
    port
        The daemon's port inside the guest.
    region
        AWS region. Defaults to AWS_REGION, then us-east-1.
    json
        Emit the typed JSON envelope on stdout instead of human output.
    dense
        Token-lean output.
    quiet
        Suppress progress on stderr.
    """
    out = make_output(as_json=json, dense=dense, quiet=quiet)
    infra = resolve_infra(region, None, None, None)
    box = _attached_box(infra, microvm_id, port=port)
    # `resume` needs a session to rebind, and the library refuses without one. Built
    # from the caller's identifiers rather than minted, which is the same reuse the
    # measurement licenses: the token survived the freeze.
    box.session = attach_session(
        infra,
        endpoint=endpoint,
        agent_token=agent_token,
        microvm_id=microvm_id,
        port=port,
    )
    out.progress(f"resuming {microvm_id}")
    session = box.resume(timeout=timeout)
    kind, _ = RESPONSE_TYPES["resume"]
    out.emit(
        ok_envelope(
            kind, {"microvmId": microvm_id, "state": "RUNNING", "endpoint": session.endpoint}
        ),
        (
            f"{microvm_id}\tRUNNING\t{session.endpoint}"
            if dense
            else f"{microvm_id} is RUNNING at {session.endpoint}"
        ),
    )


@app.command
def terminate(
    microvm_id: str,
    /,
    *,
    image_identifier: str | None = None,
    image_name: str | None = None,
    delete_image: bool = False,
    port: int | None = None,
    region: str | None = None,
    json: bool = False,
    dense: bool = False,
    quiet: bool = False,
) -> None:
    """Tear down a MicroVM, and optionally its image and build log group.

    Never raises on a teardown failure — it reports the identifier instead. An
    identifier you can read is the only remedy for a resource that would not delete,
    and an image in `CREATING` cannot be deleted at all.

    Parameters
    ----------
    microvm_id
        The MicroVM to terminate.
    image_identifier
        The image to delete, if `--delete-image` is given.
    image_name
        The image's name, needed to name its build log group. The service created
        that group, so `terraform destroy` never removes it.
    delete_image
        Also delete the image and its build log group.
    port
        The daemon's port inside the guest.
    region
        AWS region. Defaults to AWS_REGION, then us-east-1.
    json
        Emit the typed JSON envelope on stdout instead of human output.
    dense
        Token-lean output.
    quiet
        Suppress progress on stderr.
    """
    out = make_output(as_json=json, dense=dense, quiet=quiet)
    infra = resolve_infra(region, None, None, None)
    box = _attached_box(infra, microvm_id, port=port)
    if delete_image:
        if not image_identifier or not image_name:
            raise CliError(
                Exit.INVALID_ARG,
                "--delete-image needs both --image-identifier and --image-name",
                suggestions=[
                    "the name is what locates the build log group the service created; "
                    "the id alone cannot name it"
                ],
            )
        from .sandbox import Image

        box.image = Image(identifier=image_identifier, name=image_name)

    out.progress(f"terminating {microvm_id}")
    leaked: list[str] = []
    box.terminate(delete_image=False, delete_log_group=delete_image)
    if delete_image and image_identifier and not box.delete_image():
        leaked.append(image_identifier)
        out.warn(
            f"could not delete image {image_identifier} — it is still billing storage. An "
            "image in CREATING cannot be deleted at all (docs/PLATFORM.md, '`clientToken` is "
            "a permanent idempotency key')."
        )
    kind, _ = RESPONSE_TYPES["terminate"]
    out.emit(
        ok_envelope(
            kind,
            {
                "microvmId": microvm_id,
                "imageIdentifier": image_identifier,
                "leaked": leaked,
            },
        ),
        (
            f"{microvm_id}\t{','.join(leaked)}"
            if dense
            else "\n".join([f"terminated {microvm_id}", *(f"LEAKED: {x}" for x in leaked)])
        ),
    )
    if leaked:
        raise AlreadyReported(Exit.PLATFORM)


@app.command
def ls(
    *,
    state_dir: Path | None = None,
    json: bool = False,
    dense: bool = False,
    quiet: bool = False,
) -> None:
    """List what this CLI created and could not confirm it deleted.

    Reads the local ledger rather than asking AWS. Deliberately: the question it
    answers is "what did I leave behind", and the resources worth asking about are
    the ones a killed process never got to report — which no `ListMicrovms` call can
    attribute back to a command that died.

    Parameters
    ----------
    state_dir
        Where the ledgers live. Defaults to $MICROVM_STATE_DIR or ~/.microvm/runs.
    json
        Emit the typed JSON envelope on stdout instead of human output.
    dense
        Token-lean output.
    quiet
        Suppress progress on stderr.
    """
    out = make_output(as_json=json, dense=dense, quiet=quiet)
    root = state_dir or STATE_DIR
    runs: list[dict[str, Any]] = []
    if root.exists():
        for path in sorted(root.glob("*.json")):
            try:
                runs.append(jsonlib.loads(path.read_text()))
            except (OSError, ValueError):
                # A truncated ledger is what a process killed mid-write leaves, and
                # it is exactly the case this command exists for — so it is reported
                # as unreadable rather than skipped silently.
                runs.append({"runId": path.stem, "unreadable": str(path)})
    kind, _ = RESPONSE_TYPES["ls"]
    if dense:
        text = "\n".join(
            f"{r.get('runId', '')}\t{r.get('microvmId') or ''}\t{','.join(r.get('leaked') or [])}"
            for r in runs
        )
    elif not runs:
        text = "nothing outstanding"
    else:
        text = "\n".join(
            f"{r.get('runId', '')}  microvm={r.get('microvmId')} image={r.get('imageIdentifier')} "
            f"leaked={','.join(r.get('leaked') or []) or '-'}"
            for r in runs
        )
    out.emit(ok_envelope(kind, {"runs": runs}), text)


@app.command
def logs(
    image_name: str,
    /,
    *,
    limit: int = 200,
    region: str | None = None,
    json: bool = False,
    dense: bool = False,
    quiet: bool = False,
) -> None:
    """Read an image's build and daemon logs from CloudWatch.

    The group is `/aws/lambda-microvms/<image-name>`, which is derived from the name
    rather than asked for: a build role granted the plausible-but-wrong
    `/aws/lambda/microvms/*` produces builds that write no logs at all, and every
    failure then reads `reason=unknown`.

    Parameters
    ----------
    image_name
        The image whose log group to read.
    limit
        How many events to read per stream.
    region
        AWS region. Defaults to AWS_REGION, then us-east-1.
    json
        Emit the typed JSON envelope on stdout instead of human output.
    dense
        Token-lean output.
    quiet
        Suppress progress on stderr.
    """
    out = make_output(as_json=json, dense=dense, quiet=quiet)
    infra = resolve_infra(region, None, None, None)
    from .sandbox import BUILD_LOG_GROUP_PREFIX, Image, _client

    group = Image(identifier="", name=image_name).build_log_group
    client = _client("logs", infra.region)
    lines: list[str] = []
    streams = client.describe_log_streams(
        logGroupName=group, orderBy="LastEventTime", descending=True, limit=5
    )
    for stream in streams.get("logStreams", []):
        events = client.get_log_events(
            logGroupName=group,
            logStreamName=stream["logStreamName"],
            limit=limit,
            startFromHead=False,
        )
        lines.extend(
            str(event.get("message", "")).rstrip("\n") for event in events.get("events", [])
        )
    if not lines:
        out.warn(
            f"{group} contains no events. The build role must grant logs on "
            f"{BUILD_LOG_GROUP_PREFIX}/*; a policy granting /aws/lambda/microvms/* instead "
            "produces builds with no logs and failures that read as unknown."
        )
    kind, _ = RESPONSE_TYPES["logs"]
    out.emit(
        ok_envelope(kind, {"logGroup": group, "lines": lines}),
        "\n".join(lines) if lines else f"{group}: no events",
    )


@app.command
def cost(
    *,
    estimate: bool = False,
    compare: bool = False,
    memory: MemoryMib = 2048,
    running_sec: float = 0.0,
    suspended_sec: float = 0.0,
    build_sec: float = 0.0,
    image_gb: float | None = None,
    cycles: int = 1,
    hold_sec: float = 3600.0,
    json: bool = False,
    dense: bool = False,
    quiet: bool = False,
) -> None:
    """What a run cost, or what a plan will cost. Every figure labelled.

    Dollars are estimates derived from published rates and never an invoice — only
    Cost Explorer knows the bill. Seconds you supply from a real run are labelled
    measured; `--estimate` labels every duration projected, so an estimate cannot
    print as a report of something that happened. A line item with no published rate
    reads `unpriced`, never `$0.00`.

    Parameters
    ----------
    estimate
        Treat the durations as a plan rather than as timings.
    compare
        Also print running versus suspended for the same hold, with the break-even.
    memory
        Baseline MiB, selecting a documented size class.
    running_sec
        Seconds the VM spent, or will spend, RUNNING. Billed at baseline whether or
        not anything is executing.
    suspended_sec
        Seconds spent suspended. Storage only — no compute line at all.
    build_sec
        Seconds the image build took. Appears as an unpriced line: AWS does not
        publish whether the server-side build is billed.
    image_gb
        Image size in GB. Adds storage with its one-week minimum retention, which is
        the floor of a create-and-destroy suite.
    cycles
        Suspend/resume cycles, each paying a snapshot write plus a read.
    hold_sec
        The hold to compare running against suspended over.
    json
        Emit the typed JSON envelope on stdout instead of human output.
    dense
        Token-lean output.
    quiet
        Suppress progress on stderr.
    """
    out = make_output(as_json=json, dense=dense, quiet=quiet)
    if estimate:
        report = estimate_run(
            size=int(memory),
            running_seconds=running_sec,
            suspended_seconds=suspended_sec,
            image_gb=image_gb,
            suspend_resume_cycles=cycles,
            launched=bool(running_sec or image_gb),
        )
    else:
        report = run_report(
            size=int(memory),
            running=Duration.measured(running_sec) if running_sec else None,
            suspended=Duration.measured(suspended_sec) if suspended_sec else None,
            image_build=Duration.measured(build_sec) if build_sec else None,
            image_gb=image_gb,
            suspend_resume_cycles=cycles,
            launched=bool(running_sec or image_gb),
            label="run",
        )
    if report.staleness:
        out.warn(report.staleness)

    comparison = None
    comparison_text = ""
    if compare:
        residency = compare_residency(size=int(memory), hold_seconds=hold_sec, cycles=cycles)
        comparison = {
            "holdSeconds": residency.hold.seconds,
            "cycles": residency.cycles,
            "running": report_to_dict(residency.running),
            "suspended": report_to_dict(residency.suspended),
            "ratio": str(residency.ratio),
            "perCycleUsd": str(residency.per_cycle.amount),
            "breakEvenSeconds": residency.break_even_seconds,
            "render": residency.render(),
        }
        comparison_text = residency.render()

    kind, _ = RESPONSE_TYPES["cost"]
    payload = {"report": report_to_dict(report), "comparison": comparison}
    if dense:
        text = "\n".join(
            f"{item.phase.value}\t{item.unit}\t"
            f"{item.amount.amount if isinstance(item.amount, EstimatedUSD) else 'unpriced'}"
            for item in report.items
        )
    else:
        text = "\n".join(x for x in (report.render(), comparison_text) if x)
    out.emit(ok_envelope(kind, payload), text)


@app.command
def doctor(
    *,
    binary: Path | None = None,
    infra_dir: Path | None = None,
    region: str | None = None,
    bucket: str | None = None,
    build_role_arn: str | None = None,
    execution_role_arn: str | None = None,
    json: bool = False,
    dense: bool = False,
    quiet: bool = False,
) -> None:
    """Check every prerequisite and say which one is wrong.

    The command that saves an hour on a first attempt. Credentials, the region the
    connector ARN is interpolated into, the three Terraform outputs, whether the
    stack is applied, and whether the daemon binary is aarch64 — that last one being
    the failure that otherwise surfaces as a run-hook timeout, 45 minutes into a
    build, saying nothing about architecture.

    Parameters
    ----------
    binary
        The agentd binary to check the architecture of.
    infra_dir
        The Terraform stack directory. Defaults to ./conformance/infra.
    region
        AWS region. Defaults to AWS_REGION, then us-east-1.
    bucket
        S3 bucket. Defaults to $MICROVM_BUCKET.
    build_role_arn
        Build role. Defaults to $MICROVM_BUILD_ROLE_ARN.
    execution_role_arn
        Execution role. Defaults to $MICROVM_EXECUTION_ROLE_ARN.
    json
        Emit the typed JSON envelope on stdout instead of human output.
    dense
        Token-lean output.
    quiet
        Suppress progress on stderr.
    """
    out = make_output(as_json=json, dense=dense, quiet=quiet)
    infra = resolve_infra(region, bucket, build_role_arn, execution_role_arn)
    checks = run_doctor(
        infra,
        binary=binary,
        infra_dir=infra_dir or Path("conformance/infra"),
    )
    healthy = all(check.ok for check in checks if check.fatal)
    kind, _ = RESPONSE_TYPES["doctor"]
    out.emit(
        ok_envelope(kind, {"checks": [c.as_dict() for c in checks], "ok": healthy}),
        render_doctor(checks, dense=dense),
    )
    if not healthy:
        # A success envelope with `ok: false`, because the *check* succeeded — it
        # found what was wrong, which is the command's whole job. The exit code is
        # what a script branches on, and PRECONDITION is what it means.
        raise AlreadyReported(Exit.PRECONDITION)


@app.command
def manifest(
    *,
    json: bool = True,
    dense: bool = False,
    quiet: bool = False,
) -> None:
    """Emit the whole command surface, its exit codes, and its envelope schema.

    Derived from the registered command tree rather than written down, so it cannot
    drift from what the CLI actually accepts. Defaults to JSON: the only consumer
    that asks for a manifest is one that parses it.

    Parameters
    ----------
    json
        Emit JSON. On by default here, unlike every other command.
    dense
        Token-lean output.
    quiet
        Suppress progress on stderr.
    """
    out = make_output(as_json=json, dense=dense, quiet=quiet)
    built = build_manifest()
    kind, _ = RESPONSE_TYPES["manifest"]
    out.emit(ok_envelope(kind, built), render_manifest(built, dense=dense))


# -- dispatch ----------------------------------------------------------------


def dispatch(argv: Sequence[str], *, out: Output | None = None) -> int:
    """Runs one invocation and returns its exit code. The importable entry point.

    Returns rather than exits, and takes its argv rather than reading `sys.argv`, so
    the whole surface is callable from a test — which is the parity rule: anything a
    consumer can do by shelling out, they can do by importing.

    The `--json` and `--dense` flags are read off the raw tokens for the error path
    because a parse failure never reaches a handler, and an agent that asked for JSON
    must get JSON even when what it gets is an argument error.
    """
    global _ACTIVE_OUTPUT
    tokens = list(argv)
    reporter = out or Output(
        as_json="--json" in tokens,
        dense="--dense" in tokens,
        quiet="--quiet" in tokens,
    )
    previous = _ACTIVE_OUTPUT
    _ACTIVE_OUTPUT = reporter
    try:
        # `print_error=False` because cyclopts writes its parse errors to a Rich
        # console of its own, which is a second thing on stderr that is not an
        # envelope — and under `--json` an agent would get a boxed table it cannot
        # parse instead of ERR_INVALID_ARG. The message is not lost: `classify` puts
        # the exception's own text in the envelope.
        app(tokens, exit_on_error=False, print_error=False, help_on_error=False)
    except AlreadyReported as signal:
        return int(signal.exit_code)
    except SystemExit as exc:
        # cyclopts exits zero for `--help` and `--version`, which are successes. It
        # also converts a KeyboardInterrupt raised inside a handler into
        # SystemExit(130) before this frame sees it, so the interrupt has to be
        # recovered from the code rather than caught as itself — otherwise the one
        # failure AC-5-6 is about reports as an unmapped 130.
        code = int(exc.code or 0)
        if code == _SIGINT_EXIT_CODE:
            error = CliError(Exit.INTERRUPTED, "interrupted")
            reporter.emit(error_envelope(error), render_error(error))
            return int(Exit.INTERRUPTED)
        return code
    except BaseException as exc:  # noqa: BLE001 - the classifier is exhaustive by design
        error = classify(exc)
        reporter.emit(error_envelope(error), render_error(error))
        return int(error.exit_code)
    finally:
        # Restored rather than cleared, so a nested `dispatch` — which is how the
        # test suite drives the surface — does not strand the outer invocation's
        # streams and start printing to the real stdout mid-test.
        _ACTIVE_OUTPUT = previous
    return int(Exit.OK)


def main() -> None:
    """The console-script entry point."""
    raise SystemExit(dispatch(sys.argv[1:]))
