"""The `microvm` CLI: the exit-code contract, the envelope, and the two thinness guards.

No AWS and no network. Every test patches the library seam — `cli.open_sandbox` and
`cli.attach_session` — which is the same seam AC-5-4's behavioral guard patches to
raise. That is not a coincidence: if a command could reach AWS around those two
functions, the tests here would pass while the guard failed, and the guard is the
one that matters.

Two properties get asserted more than once on purpose, because each has a cheap
version that cannot fail:

* "the envelope is present" passes for a CLI that also writes progress to stdout.
  So the assertion is that stdout parses as *exactly one* JSON document with
  progress logging on.
* "the command failed with the seam patched" passes for a CLI that calls the library
  and then also calls boto3 directly. So it is paired with a static import check.
"""

from __future__ import annotations

import ast
import io
import json
import pathlib
from typing import Any

import pytest

from microvms_agentd import cli


class Recorder:
    """A stand-in for `Sandbox` that records calls and answers plausibly.

    Not a `Mock`: the shapes it returns are the shapes the library returns, so a
    handler that reads `image.build_log_group` or `session.endpoint` is exercised
    rather than handed an auto-attribute that silently satisfies anything.
    """

    def __init__(self) -> None:
        self.calls: list[tuple[str, dict[str, Any]]] = []
        self.microvm_id: str | None = None
        self.agent_token: str | None = None
        self.image: Any = None
        self.session: Any = None
        self.image_deletes = 0
        self.delete_image_result = True
        self.exec_exit_code = 0
        self.suspend_state = "SUSPENDED"

    def _record(self, name: str, kwargs: dict[str, Any]) -> None:
        self.calls.append((name, dict(kwargs)))

    def named(self) -> list[str]:
        return [name for name, _ in self.calls]

    def kwargs(self, name: str) -> dict[str, Any]:
        return next(kw for called, kw in self.calls if called == name)

    def build_image(self, **kw: Any) -> Any:
        self._record("build_image", kw)
        from microvms_agentd.sandbox import Image
        from microvms_agentd.sizing import size_class_for

        self.image = Image(
            identifier="img-1", name=kw["name"], size=size_class_for(kw.get("memory_mib", 2048))
        )
        return self.image

    def run(self, **kw: Any) -> Any:
        self._record("run", kw)
        self.microvm_id = "mvm-1"
        self.agent_token = "tok-1"
        self.session = FakeSession(exit_code=self.exec_exit_code)
        return self.session

    def suspend(self, **kw: Any) -> str:
        self._record("suspend", kw)
        return self.suspend_state

    def resume(self, **kw: Any) -> Any:
        self._record("resume", kw)
        self.session = self.session or FakeSession()
        return self.session

    def terminate(self, **kw: Any) -> None:
        self._record("terminate", kw)

    def delete_image(self, **kw: Any) -> bool:
        self._record("delete_image", kw)
        self.image_deletes += 1
        return self.delete_image_result


class FakeSession:
    """The `Session` surface the CLI touches, and nothing else."""

    def __init__(self, *, exit_code: int = 0, endpoint: str = "https://abc.example") -> None:
        self.endpoint = endpoint
        self.exit_code = exit_code
        self.commands: list[str] = []

    def wait_until_ready(self, **_: Any) -> Any:
        return None

    def run_sync(self, command: str, **_: Any) -> Any:
        self.commands.append(command)
        from microvms_agentd.models import ExecResult, Phase

        return ExecResult(
            exec_id="x-1",
            phase=Phase.ACKED,
            exit_code=self.exit_code,
            stdout="hello\n",
            stderr="",
        )


@pytest.fixture
def binary(tmp_path: pathlib.Path) -> pathlib.Path:
    """A minimal aarch64 ELF header, so `doctor`'s architecture check has something real.

    Sixteen bytes of truth beats a mock: the check reads `e_machine` out of the
    header, and a test against a fake that returns "aarch64" would not notice the
    offset arithmetic being wrong.
    """
    path = tmp_path / "agentd"
    header = bytearray(20)
    header[0:4] = b"\x7fELF"
    header[4] = 2  # 64-bit
    header[5] = 1  # little-endian
    header[18:20] = (0xB7).to_bytes(2, "little")  # EM_AARCH64
    path.write_bytes(bytes(header))
    return path


@pytest.fixture
def seam(monkeypatch: pytest.MonkeyPatch) -> Recorder:
    """Patches both library seams to one recorder, and the state dir to a temp path."""
    recorder = Recorder()
    monkeypatch.setattr(cli, "open_sandbox", lambda *a, **k: recorder)
    monkeypatch.setattr(cli, "attach_session", lambda *a, **k: FakeSession())
    return recorder


@pytest.fixture
def infra_env(monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
    monkeypatch.setenv("MICROVM_BUCKET", "bucket-1")
    monkeypatch.setenv("MICROVM_BUILD_ROLE_ARN", "arn:build")
    monkeypatch.setenv("MICROVM_EXECUTION_ROLE_ARN", "arn:exec")
    monkeypatch.setattr(cli, "STATE_DIR", tmp_path / "runs")


def invoke(argv: list[str]) -> tuple[int, str, str]:
    """Runs one invocation with both streams captured, returning them separately.

    Separately is the point: every stdout assertion in this file is also an
    assertion that the progress lines went elsewhere.
    """
    stdout, stderr = io.StringIO(), io.StringIO()
    out = cli.Output(
        as_json="--json" in argv,
        dense="--dense" in argv,
        quiet="--quiet" in argv,
        stdout=stdout,
        stderr=stderr,
    )
    code = cli.dispatch(argv, out=out)
    return code, stdout.getvalue(), stderr.getvalue()


# -- AC-5-4 guard 1: the static check ----------------------------------------


#: Modules under the CLI package. One entry today, and the walk is over the package
#: rather than over this list so a `cli/` subpackage would be covered without anyone
#: remembering to add it.
def _cli_modules() -> list[pathlib.Path]:
    import microvms_agentd

    root = pathlib.Path(microvms_agentd.__file__).parent
    return sorted(p for p in root.glob("cli*.py")) + sorted(root.glob("cli/**/*.py"))


#: Transports a second implementation would need. `boto3` and `botocore` are the
#: control plane; `httpx` is the endpoint proxy. A CLI that reimplements either has
#: to import one of these — there is no third way to put a byte on the wire.
_FORBIDDEN_IMPORTS = frozenset({"boto3", "botocore", "httpx", "urllib3", "requests", "aiohttp"})

#: Control-plane operation names. Present so the check catches the subtler shape: a
#: module that gets a client *from* the library and then calls an operation the
#: library does not wrap is still a second implementation, and it imports nothing.
_OPERATION_STRINGS = (
    "create_microvm_image",
    "run_microvm",
    "get_microvm",
    "suspend_microvm",
    "resume_microvm",
    "terminate_microvm",
    "delete_microvm_image",
    "create_microvm_auth_token",
    "create_microvm_shell_auth_token",
    "CreateMicrovmImage",
    "RunMicrovm",
    "CreateMicrovmAuthToken",
)


def test_the_cli_imports_no_transport() -> None:
    # Guard 1 of AC-5-4's pair. A CLI that reimplements the control plane or the
    # endpoint proxy must import a transport to do it, so the absence of the import
    # is evidence — weak evidence, which is why guard 2 exists. Parsed with `ast`
    # rather than grepped, so `import boto3` inside a function body is caught and a
    # mention in a docstring or a comment is not.
    for path in _cli_modules():
        tree = ast.parse(path.read_text())
        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                names = [alias.name.split(".")[0] for alias in node.names]
            elif isinstance(node, ast.ImportFrom):
                names = [(node.module or "").split(".")[0]]
            else:
                continue
            offending = _FORBIDDEN_IMPORTS & set(names)
            assert not offending, f"{path.name}:{node.lineno} imports {offending}"


def _code_identifiers(tree: ast.Module) -> set[str]:
    """Every name the *code* uses: attributes, bare names, and string literals.

    Docstrings are excluded and comments never reach the AST at all, which is the
    difference between a guard and a spell-checker: the traps this CLI exists to
    explain are named in a dozen comments, and a text search over the source flags
    every one of them. What matters is whether an operation is *invoked* — which
    means an attribute access, or a string handed to something like `getattr`.
    """
    docstrings = {
        node.body[0].value
        for node in ast.walk(tree)
        if isinstance(node, ast.Module | ast.FunctionDef | ast.AsyncFunctionDef | ast.ClassDef)
        and node.body
        and isinstance(node.body[0], ast.Expr)
        and isinstance(node.body[0].value, ast.Constant)
        and isinstance(node.body[0].value.value, str)
    }
    found: set[str] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Attribute):
            found.add(node.attr)
        elif isinstance(node, ast.Name):
            found.add(node.id)
        elif (
            isinstance(node, ast.Constant)
            and isinstance(node.value, str)
            and node not in docstrings
        ):
            found.add(node.value)
    return found


def test_the_cli_calls_no_control_plane_operation() -> None:
    # The half of guard 1 that survives a re-export. Dropping `import boto3` and
    # calling `library._client(...).run_microvm(...)` imports nothing forbidden and
    # is still a second implementation — but it has to name the operation to invoke
    # it, and this is that check.
    #
    # Checked against the AST rather than the text so the guard tests behavior
    # instead of vocabulary. Written the naive way first, as a substring search, it
    # went red immediately — on a comment explaining *why* a region check is local.
    # A guard that fires on its own documentation gets deleted, and then guard 1 is
    # half as strong as it reads.
    for path in _cli_modules():
        used = _code_identifiers(ast.parse(path.read_text()))
        offending = used & set(_OPERATION_STRINGS)
        assert not offending, (
            f"{path.name} invokes the control-plane operation(s) {sorted(offending)}; the library "
            "owns every call, and a CLI that names one has grown a second path to AWS"
        )


def test_the_cli_dispatches_no_method_by_computed_name() -> None:
    # The third leg of guard 1, and it exists because the first two were defeated on
    # purpose during review. Assembling the operation name at runtime —
    # `getattr(client, "suspend" + "_" + "microvm")` — imports nothing forbidden and
    # writes no operation literal, so it passes both other halves. Guard 2 does not
    # catch it either: the call still fails when the seam is patched, so "every
    # command failed" stays true while the command has stopped being a thin layer.
    #
    # The rule that closes it: a thin layer never *calls* something it looked up by a
    # computed name. Every call it makes is one it can write down.
    #
    # Scoped to a computed lookup that is immediately invoked — `getattr(x, expr)(...)`
    # — rather than to `getattr` generally. Written the broad way first it went red on
    # `Infra.require`, which reads its own dataclass fields by name to report every
    # missing value at once. That is reflection over local state, not a call to
    # anything, and a guard that forbade it would be paid for by making a genuinely
    # better error message impossible.
    for path in _cli_modules():
        for node in ast.walk(ast.parse(path.read_text())):
            if not isinstance(node, ast.Call):
                continue
            inner = node.func
            if not isinstance(inner, ast.Call):
                continue
            callee = inner.func
            if not (isinstance(callee, ast.Name) and callee.id == "getattr"):
                continue
            # A literal name is fine — that is an attribute access with a default.
            name_arg = inner.args[1] if len(inner.args) > 1 else None
            assert isinstance(name_arg, ast.Constant) and isinstance(name_arg.value, str), (
                f"{path.name}:{node.lineno} calls a method looked up by computed name; a thin "
                "layer over the library never needs to build a method name, and building one is "
                "how a control-plane call hides from every static check"
            )


# -- AC-5-4 guard 2: the behavioral check ------------------------------------

#: Every AWS-touching command: its argv, and the CLI-level seam it must go through.
#: The seam is named per command rather than left implicit, because "it failed" and
#: "it went through the seam" are different claims and only the second one is what
#: AC-5-4 asks for. `logs` names `_client` because it needs a CloudWatch client and
#: no Sandbox — the library's own lazy factory is the seam there.
ALL_COMMANDS: tuple[tuple[str, list[str], str], ...] = (
    ("run", ["run", "BINARY", "--exec", "true"], "open_sandbox"),
    ("build", ["build", "BINARY"], "open_sandbox"),
    (
        "exec",
        ["exec", "true", "--endpoint", "e", "--agent-token", "t", "--microvm-id", "m"],
        "attach_session",
    ),
    ("suspend", ["suspend", "mvm-1"], "open_sandbox"),
    ("resume", ["resume", "mvm-1", "--endpoint", "e", "--agent-token", "t"], "open_sandbox"),
    ("terminate", ["terminate", "mvm-1"], "open_sandbox"),
    ("logs", ["logs", "image-1"], "_client"),
)

#: The commands that reach neither seam, and why each is legitimately local. Listed
#: explicitly rather than skipped by a naming rule, so a *new* AWS-touching command
#: is covered by guard 2 by default and only leaves the net by someone writing its
#: name here with a reason.
LOCAL_ONLY = {
    "ls": "reads the local ledger; the whole point is that AWS cannot attribute a dead run",
    "cost": "arithmetic over a pinned rate table; no account is involved",
    "manifest": "introspects the command tree",
    "doctor": "reports what is missing, so it must run with nothing configured",
}


@pytest.mark.parametrize(
    "name,argv,expected_seam", ALL_COMMANDS, ids=[n for n, _, _ in ALL_COMMANDS]
)
def test_every_command_goes_through_the_library_seam(
    name: str,
    argv: list[str],
    expected_seam: str,
    binary: pathlib.Path,
    infra_env: None,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Guard 2 of AC-5-4's pair, and the one guard 1 cannot replace: a CLI that calls
    # the library *and then also* calls boto3 directly imports nothing extra at the
    # CLI layer and passes the static check.
    #
    # Three assertions, and the third was added after the second was defeated on
    # purpose. Replacing `open_sandbox(...)` with a direct `Sandbox(...)` — no new
    # import, no operation name, invisible to guard 1 — still *fails* here, because
    # the library's own client factory is patched too. So "it failed" and even "it
    # failed with the sentinel" both stayed true while the seam had been bypassed.
    # What distinguishes them is whether the CLI-level seam was entered at all, which
    # is what the third assertion records.
    sentinel = "seam-was-patched-a1b2c3"
    entered: list[str] = []

    def refusing(seam_name: str) -> Any:
        def refuse(*_: Any, **__: Any) -> Any:
            entered.append(seam_name)
            raise RuntimeError(sentinel)

        return refuse

    monkeypatch.setattr(cli, "open_sandbox", refusing("open_sandbox"))
    monkeypatch.setattr(cli, "attach_session", refusing("attach_session"))
    # Patched at its source as well, so there is no path to AWS this test leaves
    # open — a command that reached around both CLI seams must still not make a call.
    import microvms_agentd.sandbox as sandbox_mod

    monkeypatch.setattr(sandbox_mod, "_client", refusing("_client"))

    code, stdout, _ = invoke([*[binary.as_posix() if a == "BINARY" else a for a in argv], "--json"])
    assert code != int(cli.Exit.OK), f"{name} succeeded with every library seam patched to raise"
    envelope = json.loads(stdout)
    assert envelope["status"] == "error"
    assert sentinel in envelope["error"], (
        f"{name} failed, but not with the patched seam's error — it reached AWS another way, "
        f"or failed for an unrelated reason: {envelope['error']!r}"
    )
    assert expected_seam in entered, (
        f"{name} failed without entering {expected_seam}; it reached the control plane by "
        f"constructing its own client instead of going through the library seam (entered: "
        f"{entered or 'nothing'})"
    )


def test_the_behavioral_guard_covers_every_command_that_touches_aws() -> None:
    # The guard above is a parametrized list, and a list is exactly the thing that
    # goes stale when a twelfth command lands. So the list is checked against the
    # registered command tree: a new command must either be exercised by guard 2 or
    # be named in LOCAL_ONLY with a reason.
    registered = {name for name in cli.app if name not in cli._META_COMMANDS}
    covered = {name for name, _, _ in ALL_COMMANDS} | set(LOCAL_ONLY)
    assert registered == covered, (
        f"commands neither guarded nor declared local: {sorted(registered - covered)}; "
        f"declared but not registered: {sorted(covered - registered)}"
    )


# -- AC-5-1: one stable exit code per failure class --------------------------

#: Each induced failure and the code it must earn. The four the AC names, plus the
#: three that separate a bug in this CLI from a platform failure. Anchored on the
#: library's own message literals, which is deliberate: those strings are what the
#: operator reads, and a reworded one has changed the contract's evidence.
INDUCED_FAILURES: tuple[tuple[str, BaseException, cli.Exit, str], ...] = (
    (
        "wedged build",
        RuntimeError(
            "build never scheduled after 240s: all builds still PENDING. This is the "
            "clientToken replay signature — the image is wedged and cannot be deleted."
        ),
        cli.Exit.BUILD_WEDGED,
        "`clientToken` is a permanent idempotency key",
    ),
    (
        "terminal state before RUNNING",
        RuntimeError(
            "microvm mvm-1 reached TERMINATED before RUNNING: Run lifecycle hook "
            "returned HTTP status 400."
        ),
        cli.Exit.LAUNCH_DIED,
        "`runHookPayload` arrives wrapped, not as the body",
    ),
    (
        "expired suspended window",
        RuntimeError(
            "microvm mvm-1 has been suspended 301s, past the 300s suspendedDurationSeconds "
            "window set at launch"
        ),
        cli.Exit.WINDOW_CLOSED,
        "`idlePolicy`",
    ),
    (
        "mint failure",
        __import__("microvms_agentd").AuthTokenMintError("could not mint a proxy auth token"),
        cli.Exit.RETRYABLE,
        "Endpoint authentication",
    ),
    (
        "wrong agent token",
        __import__("microvms_agentd").Unauthorized("GET /v1/exec/x -> 401", status=401),
        cli.Exit.CREDENTIALS,
        "",
    ),
    (
        "off-table size class",
        ValueError("minimumMemoryInMiB=1500 is not a documented size class baseline"),
        cli.Exit.INVALID_ARG,
        "",
    ),
    (
        "a bug in this CLI",
        AttributeError("'NoneType' object has no attribute 'identifier'"),
        cli.Exit.UNEXPECTED,
        "",
    ),
)


@pytest.mark.parametrize(
    "label,exception,expected,finding",
    INDUCED_FAILURES,
    ids=[label for label, _, _, _ in INDUCED_FAILURES],
)
def test_each_failure_class_earns_its_own_exit_code(
    label: str,
    exception: BaseException,
    expected: cli.Exit,
    finding: str,
    binary: pathlib.Path,
    infra_env: None,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Table-driven per AC-5-1. Each row asserts three things at once, and the second
    # two are what make collapsing codes impossible: the integer, the string code
    # beside it, and the docs/PLATFORM.md finding. A CLI that mapped every
    # RuntimeError to one code would satisfy "it exited non-zero" and fail here.
    def refuse(*_: Any, **__: Any) -> Any:
        raise exception

    monkeypatch.setattr(cli, "open_sandbox", refuse)
    code, stdout, _ = invoke(["run", binary.as_posix(), "--exec", "true", "--json"])

    assert code == int(expected), f"{label} exited {code}, expected {int(expected)}"
    envelope = json.loads(stdout)
    assert envelope["exitCode"] == int(expected), "the envelope's code must match the process's"
    spec = next(row for row in cli.EXIT_TABLE if row.exit is expected)
    assert envelope["code"] == str(spec.code)
    assert envelope["finding"] == finding, f"{label} must name the finding {finding!r}"


def test_no_two_failure_classes_share_a_code() -> None:
    # The property the table above can only sample. Every row of the contract is
    # distinct in both columns, so a future edit that reuses ERR_PLATFORM for two
    # meanings fails here rather than at whatever call site depended on the split.
    exits = [row.exit for row in cli.EXIT_TABLE]
    codes = [row.code for row in cli.EXIT_TABLE if row.code is not None]
    assert len(exits) == len(set(exits))
    assert len(codes) == len(set(codes))
    assert {int(e) for e in cli.Exit} == set(map(int, exits)), "every Exit member needs a row"


def test_an_unhandled_exception_never_borrows_a_handled_code() -> None:
    # The half of AC-5-1 that is about honesty rather than granularity. A bug in this
    # CLI reported as ERR_PLATFORM sends the reader to AWS for something in this
    # file, and the generic `except Exception` that produces it is the easiest
    # regression in the module to write.
    class Surprise(Exception):
        pass

    handled = {int(row.exit) for row in cli.EXIT_TABLE if row.exit is not cli.Exit.UNEXPECTED}
    error = cli.classify(Surprise("something nobody predicted"))
    assert error.exit_code is cli.Exit.UNEXPECTED
    assert int(error.exit_code) not in handled - {int(cli.Exit.UNEXPECTED)}
    assert error.code is cli.Code.ERR_UNEXPECTED


def test_a_failing_workload_is_not_a_failing_platform(
    binary: pathlib.Path, infra_env: None, seam: Recorder
) -> None:
    # A test suite exiting 1 inside a healthy sandbox is the single most common
    # non-zero outcome of the headline command, and it must not be confusable with
    # any of the twelve ways the platform can fail. Its envelope stays `ok` —
    # everything the caller asked for is in it — and only the exit code differs.
    seam.exec_exit_code = 1
    code, stdout, _ = invoke(["run", binary.as_posix(), "--exec", "pytest", "--json"])

    assert code == int(cli.Exit.EXEC_FAILED)
    envelope = json.loads(stdout)
    assert envelope["status"] == "ok", "the sandbox worked; only the command in it failed"
    assert envelope["data"]["execExitCode"] == 1


# -- AC-5-2: exactly one envelope on stdout ----------------------------------


def test_a_failure_writes_one_json_document_with_progress_enabled(
    binary: pathlib.Path, infra_env: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The AC's own falsification, run as the test. Progress logging is *on* — no
    # --quiet — and the build fails after several progress lines have been written.
    # stdout must still parse as a single JSON document, which is what fails for a
    # CLI that prints "building image ..." to stdout. The weaker "is the envelope in
    # there" check passes for that CLI, which is why this one parses the whole stream
    # rather than searching it.
    class Failing(Recorder):
        def run(self, **kw: Any) -> Any:
            raise RuntimeError("microvm mvm-1 reached TERMINATED before RUNNING: hook 400")

    recorder = Failing()
    monkeypatch.setattr(cli, "open_sandbox", lambda *a, **k: recorder)
    code, stdout, stderr = invoke(["run", binary.as_posix(), "--exec", "true", "--json"])

    assert code == int(cli.Exit.LAUNCH_DIED)
    envelope = json.loads(stdout)  # raises if anything else reached stdout
    assert envelope["status"] == "error"
    assert stderr.strip(), "the progress lines must have gone somewhere, and stderr is where"
    assert "building image" in stderr


def test_progress_and_warnings_never_reach_stdout(
    binary: pathlib.Path, infra_env: None, seam: Recorder
) -> None:
    # The success path of the same rule, including the warning channel. A leaked
    # identifier is a warning, and a warning on stdout corrupts the envelope for the
    # one consumer most likely to be reading it — an agent that just lost a VM.
    seam.delete_image_result = False
    code, stdout, stderr = invoke(["run", binary.as_posix(), "--exec", "true", "--json"])

    assert code == int(cli.Exit.OK)
    envelope = json.loads(stdout)
    assert envelope["status"] == "ok"
    assert "warning:" in stderr
    assert "warning:" not in stdout


def test_quiet_silences_progress_but_never_a_leak(
    binary: pathlib.Path, infra_env: None, seam: Recorder
) -> None:
    # `--quiet` is for a pipeline, and a pipeline is exactly where a leaked billable
    # resource goes unnoticed. So the flag buys silence about what went right and
    # cannot buy silence about what is still costing money.
    seam.delete_image_result = False
    _, _, stderr = invoke(["run", binary.as_posix(), "--exec", "true", "--json", "--quiet"])

    assert "building image" not in stderr, "--quiet suppresses progress"
    assert "could not delete img-1" in stderr, "--quiet must not suppress a leak"


def test_every_command_emits_a_typed_envelope(
    binary: pathlib.Path, infra_env: None, seam: Recorder, tmp_path: pathlib.Path
) -> None:
    # The discriminant is what an agent branches on before parsing `data`, so every
    # command must carry one and it must be the one the manifest advertises. Checked
    # across the whole surface rather than per command, because a single handler that
    # forgot its envelope is invisible in its own test.
    invocations = {
        "run": ["run", binary.as_posix(), "--exec", "true"],
        "build": ["build", binary.as_posix()],
        "exec": ["exec", "true", "--endpoint", "e", "--agent-token", "t", "--microvm-id", "m"],
        "suspend": ["suspend", "mvm-1"],
        "resume": ["resume", "mvm-1", "--endpoint", "e", "--agent-token", "t"],
        "terminate": ["terminate", "mvm-1"],
        "ls": ["ls", "--state-dir", (tmp_path / "empty").as_posix()],
        "cost": ["cost", "--estimate", "--running-sec", "60"],
        "manifest": ["manifest"],
    }
    for name, argv in invocations.items():
        _, stdout, _ = invoke([*argv, "--json"])
        envelope = json.loads(stdout)
        assert envelope["apiVersion"] == cli.API_VERSION
        assert envelope["status"] == "ok", f"{name} did not succeed against the fake seam"
        expected, keys = cli.RESPONSE_TYPES[name]
        assert envelope["type"] == expected, (
            f"{name} advertised {expected} and emitted {envelope['type']}"
        )
        assert set(envelope["data"]) == set(keys), (
            f"{name}'s payload keys disagree with the manifest: "
            f"{sorted(set(envelope['data']) ^ set(keys))}"
        )


# -- AC-5-3: the manifest is derived, not maintained -------------------------


def test_the_manifest_matches_the_registered_command_tree() -> None:
    # A hand-kept manifest drifts the first time someone adds a flag, and its entire
    # value to an agent is that it cannot be wrong. So the assertion is against
    # cyclopts' own registration rather than against a literal.
    manifest = cli.build_manifest()
    listed = {command["name"] for command in manifest["commands"]}
    registered = {name for name in cli.app if name not in cli._META_COMMANDS}
    assert listed == registered
    assert len(listed) == 11, "the plan names eleven subcommands"


def test_the_manifest_carries_every_exit_code() -> None:
    # AC-5-3's cross-check against AC-5-1: adding a code without it appearing here
    # must go red. It does, because both read the same table — which is the property
    # being asserted, since a manifest that restated the codes could omit one.
    manifest = cli.build_manifest()
    listed = {row["exit"] for row in manifest["exitCodes"]}
    assert listed == {int(member) for member in cli.Exit}
    for row in manifest["exitCodes"]:
        if row["exit"] != int(cli.Exit.OK):
            assert row["code"], f"exit {row['exit']} has no machine-readable code"


def test_every_command_declares_its_response_type() -> None:
    # The manifest's `responseType` is the discriminant, and a command missing one
    # ships undescribed — the agent has to guess how to read `data`.
    registered = {name for name in cli.app if name not in cli._META_COMMANDS}
    assert set(cli.RESPONSE_TYPES) == registered
    for command in cli.build_manifest()["commands"]:
        assert command["responseType"], f"{command['name']} advertises no response type"
        assert command["responseKeys"], f"{command['name']} advertises no payload keys"


def test_the_manifest_reports_help_derived_from_the_docstrings() -> None:
    # Pattern 6: one source drives --help, the manifest, and any generated agent doc.
    # Asserted on a specific parameter rather than on "help is non-empty", because a
    # manifest that emitted the parameter *name* as its help would pass the weak
    # check and teach an agent nothing.
    run_command = next(c for c in cli.build_manifest()["commands"] if c["name"] == "run")
    keep = next(p for p in run_command["parameters"] if p["name"] == "--keep")
    assert "paying" in keep["help"], keep["help"]


# -- AC-5-5: no option accepts what the library rejects ----------------------


def test_the_memory_option_is_a_closed_set_matching_the_size_table() -> None:
    # AC-5-5 for the size class. `sizing.size_class_for` rejects an off-table
    # baseline, so an `int` option would accept 1500 and turn an S1 guard into a
    # runtime error — the CLI-shaped downgrade the AC exists to catch. Compared
    # against the size table rather than against a literal list, so a new row that
    # never reaches the Literal fails here.
    from microvms_agentd.sizing import SIZE_CLASSES

    manifest = cli.build_manifest()
    expected = {str(size.baseline_mib) for size in SIZE_CLASSES}
    for name in ("run", "build", "cost"):
        command = next(c for c in manifest["commands"] if c["name"] == name)
        memory = next(p for p in command["parameters"] if p["name"] == "--memory")
        assert memory["choices"] is not None, f"{name} --memory is not a closed set"
        assert set(memory["choices"]) == expected


def test_no_option_forwards_a_raw_capability_list_or_connector_string() -> None:
    # The other two S1 library guards, checked as an *absence*. The library replaced
    # a capability list with a boolean intent and a connector string with a closed
    # enum; a convenience `--os-capabilities` or `--connector` flag that forwarded a
    # raw value would re-open both, and the CLI is where that is most tempting.
    manifest = cli.build_manifest()
    for command in manifest["commands"]:
        names = {p["name"] for p in command["parameters"]}
        assert "--os-capabilities" not in names, (
            f"{command['name']} exposes a raw capability list; 'ALL' is the only value the API "
            "accepts, so the intent flag --repair-identity is the whole surface"
        )
        assert not names & {"--connector", "--connectors", "--network-connector"}, (
            f"{command['name']} exposes a free-form connector; the bare name is rejected as a "
            "malformed ARN, so the surface is the --egress intent"
        )
        assert not names & {"--client-token", "--token", "--idempotency-token"}, (
            f"{command['name']} accepts an idempotency token; a content-derived one wedges an "
            "image permanently, which is why the library takes none"
        )


# -- AC-5-6: an interrupt tears down and names what it could not delete ------


def test_an_interrupt_after_launch_tears_down_and_names_the_leak(
    binary: pathlib.Path, infra_env: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A CLI is the surface most likely to be killed mid-run, and an image left in
    # CREATING cannot be deleted afterward at all — so the identifier is the whole
    # remedy. Two assertions, and the AC says why: an implementation that tears down
    # silently satisfies the first and fails the second, which is the point.
    class Interrupted(Recorder):
        def run(self, **kw: Any) -> Any:
            session = super().run(**kw)
            raise KeyboardInterrupt
            return session  # unreachable; keeps the signature honest

    recorder = Interrupted()
    # The image deletion fails, which is what a wedged image does. Without that the
    # leak list is empty and the second assertion cannot fail.
    recorder.delete_image_result = False
    monkeypatch.setattr(cli, "open_sandbox", lambda *a, **k: recorder)

    code, stdout, stderr = invoke(["run", binary.as_posix(), "--exec", "true", "--json"])

    assert "terminate" in recorder.named(), "teardown must run even on an interrupt"
    assert code == int(cli.Exit.INTERRUPTED)
    # The identifier must appear in the *warning*, not merely somewhere on stderr.
    # Asserting `"img-1" in stderr` was the first version of this line and it could
    # not fail: the progress output names the image on the way in, so a teardown that
    # said nothing at all about the leak still passed.
    warnings = [line for line in stderr.splitlines() if line.startswith("warning:")]
    assert any("img-1" in line for line in warnings), (
        "an identifier the CLI could not delete must be named in a warning, since it is the "
        f"operator's only remedy; warnings were: {warnings}"
    )
    assert any("still billing" in line for line in warnings)
    envelope = json.loads(stdout)
    assert envelope["code"] == "ERR_INTERRUPTED"


def test_the_ledger_survives_the_process_so_ls_can_report_it(
    binary: pathlib.Path, infra_env: None, monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path
) -> None:
    # The identifiers are worthless to the operator if the process that held them is
    # the process that died, so they are on disk before the delete is attempted
    # rather than after. `ls` is how they are read back.
    state = tmp_path / "runs"
    monkeypatch.setattr(cli, "STATE_DIR", state)

    class Interrupted(Recorder):
        def run(self, **kw: Any) -> Any:
            super().run(**kw)
            raise KeyboardInterrupt

    recorder = Interrupted()
    recorder.delete_image_result = False
    monkeypatch.setattr(cli, "open_sandbox", lambda *a, **k: recorder)
    invoke(["run", binary.as_posix(), "--exec", "true", "--json"])

    _, stdout, _ = invoke(["ls", "--state-dir", state.as_posix(), "--json"])
    runs = json.loads(stdout)["data"]["runs"]
    assert runs, "a run with an undeleted resource must leave a ledger behind"
    assert "img-1" in runs[0]["leaked"]


def test_a_clean_run_leaves_no_ledger_behind(
    binary: pathlib.Path, infra_env: None, seam: Recorder, tmp_path: pathlib.Path, monkeypatch
) -> None:
    # The other half: a ledger file is the signal that something is outstanding, so
    # one left behind after a clean teardown would train the operator to ignore `ls`.
    state = tmp_path / "runs"
    monkeypatch.setattr(cli, "STATE_DIR", state)
    invoke(["run", binary.as_posix(), "--exec", "true", "--json"])

    _, stdout, _ = invoke(["ls", "--state-dir", state.as_posix(), "--json"])
    assert json.loads(stdout)["data"]["runs"] == []


def test_run_tears_down_by_default_and_keeps_only_when_asked(
    binary: pathlib.Path, infra_env: None, seam: Recorder
) -> None:
    # A CLI that leaks a billable VM by default is worse than no CLI, because the
    # bill arrives a month after the person forgot they ran it. So the default is
    # asserted directly, and `--keep` is asserted to be the *only* way out.
    invoke(["run", binary.as_posix(), "--exec", "true"])
    assert "terminate" in seam.named()
    assert seam.image_deletes == 1

    kept = Recorder()
    import pytest as _pytest

    with _pytest.MonkeyPatch.context() as patch:
        patch.setattr(cli, "open_sandbox", lambda *a, **k: kept)
        _, stdout, _ = invoke(["run", binary.as_posix(), "--exec", "true", "--keep"])
    assert "terminate" not in kept.named(), "--keep must not tear down"
    assert kept.image_deletes == 0
    assert "mvm-1" in stdout, "the caller now owns the identifiers and must be told them"


# -- doctor ------------------------------------------------------------------


def test_doctor_names_a_host_architecture_binary(tmp_path: pathlib.Path) -> None:
    # The failure that otherwise costs a full build cycle and then surfaces as a
    # run-hook timeout, which says nothing about architecture. Asserted against a
    # real x86-64 ELF header rather than a stub, because the check is header
    # arithmetic and a stub would not notice the offset being wrong.
    path = tmp_path / "agentd-x86"
    header = bytearray(20)
    header[0:4] = b"\x7fELF"
    header[4] = 2
    header[5] = 1
    header[18:20] = (0x3E).to_bytes(2, "little")  # EM_X86_64
    path.write_bytes(bytes(header))

    check = cli.check_binary(path)
    assert not check.ok
    assert "aarch64" in check.remedy
    assert "run-hook timeout" in check.remedy, "the remedy must name how the failure appears"


def test_doctor_accepts_an_aarch64_binary(binary: pathlib.Path) -> None:
    assert cli.check_binary(binary).ok


def test_doctor_reports_a_missing_value_rather_than_guessing(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Each of the three Terraform outputs is its own check, because "infrastructure
    # missing" sends someone to re-read a whole stack while "no execution role" sends
    # them to one output.
    for name in ("MICROVM_BUCKET", "MICROVM_BUILD_ROLE_ARN", "MICROVM_EXECUTION_ROLE_ARN"):
        monkeypatch.delenv(name, raising=False)
    infra = cli.resolve_infra(None, None, None, None, env={})
    checks = {check.name: check for check in cli.check_infra(infra)}
    assert set(checks) == {"bucket", "build-role", "execution-role"}
    for check in checks.values():
        assert not check.ok
        assert "terraform" in check.remedy


def test_doctor_treats_an_unlisted_region_as_advisory() -> None:
    # AWS adds regions faster than the constant is re-read, so a hard failure here
    # would block a caller who is right and we are stale. Warned, not failed.
    unlisted = cli.check_region(cli.Infra(region="ap-south-2"))
    assert not unlisted.ok
    assert not unlisted.fatal, "an unlisted region must not fail the whole check"
    assert cli.check_region(cli.Infra(region="us-east-1")).ok


def test_doctor_exits_precondition_when_something_is_fatally_wrong(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path
) -> None:
    # The envelope stays `ok` because the *check* succeeded — finding the problem is
    # the job. The exit code is what a script branches on.
    monkeypatch.setattr(
        cli,
        "check_credentials",
        lambda infra: cli.Check(name="credentials", ok=False, detail="no credentials"),
    )
    for name in ("MICROVM_BUCKET", "MICROVM_BUILD_ROLE_ARN", "MICROVM_EXECUTION_ROLE_ARN"):
        monkeypatch.delenv(name, raising=False)
    code, stdout, _ = invoke(["doctor", "--infra-dir", (tmp_path / "none").as_posix(), "--json"])

    envelope = json.loads(stdout)
    assert envelope["status"] == "ok", "the check ran; what it found is in the payload"
    assert envelope["data"]["ok"] is False
    assert code == int(cli.Exit.PRECONDITION)


def test_doctor_runs_with_nothing_configured_at_all(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path
) -> None:
    # The command someone runs *because* nothing works, so it must not need the thing
    # it is checking for. No credentials, no infra, no binary: it still reports.
    monkeypatch.setattr(
        cli,
        "check_credentials",
        lambda infra: cli.Check(name="credentials", ok=False, detail="NoCredentialsError"),
    )
    for name in ("MICROVM_BUCKET", "MICROVM_BUILD_ROLE_ARN", "MICROVM_EXECUTION_ROLE_ARN"):
        monkeypatch.delenv(name, raising=False)
    code, stdout, _ = invoke(["doctor", "--infra-dir", (tmp_path / "none").as_posix(), "--json"])
    assert code == int(cli.Exit.PRECONDITION)
    checks = json.loads(stdout)["data"]["checks"]
    assert len(checks) == 7, "every prerequisite is reported, not just the first failure"


# -- cost --------------------------------------------------------------------


def test_an_estimate_never_prints_as_an_invoice() -> None:
    # The label is the feature. An estimate's durations are all projected, so a report
    # of something that happened and a projection of something that might are told
    # apart by their own contents rather than by which flag produced them.
    _, stdout, _ = invoke(["cost", "--estimate", "--running-sec", "3600", "--json"])
    report = json.loads(stdout)["data"]["report"]
    assert report["estimated"] is True
    assert report["fullyMeasured"] is False, "an estimate is never measured"
    assert all(
        item["duration"]["provenance"] == "projected"
        for item in report["items"]
        if item["duration"] is not None
    )
    assert "estimate" in report["label"]


def test_a_measured_run_is_labelled_measured(
    binary: pathlib.Path, infra_env: None, seam: Recorder
) -> None:
    # And the converse, from the headline command: `run` times its own phases, so the
    # cost it reports carries MEASURED durations. Without this the two labels are
    # indistinguishable in practice and the whole distinction is decoration.
    _, stdout, _ = invoke(["run", binary.as_posix(), "--exec", "true", "--json"])
    # `run` carries the report itself under `cost`; only the `cost` command wraps one
    # alongside an optional comparison. Two shapes because the two commands answer
    # different questions, and `run`'s is always about the run that just happened.
    report = json.loads(stdout)["data"]["cost"]
    provenances = {
        item["duration"]["provenance"] for item in report["items"] if item["duration"] is not None
    }
    assert "measured" in provenances


def test_an_unpriced_line_item_never_serializes_as_zero() -> None:
    # The one arithmetic this module refuses to enable. AWS does not publish whether
    # the server-side build is billed, so a report that summed it as $0.00 would lie
    # in the direction that flatters us — and a `null` would be summed as zero by
    # anything permissive, so the key is absent entirely.
    _, stdout, _ = invoke(["cost", "--build-sec", "300", "--image-gb", "2", "--json"])
    report = json.loads(stdout)["data"]["report"]
    unpriced = [item for item in report["items"] if item["amount"]["kind"] == "unpriced"]
    assert unpriced, "an image build with no published rate must appear as a line item"
    for item in unpriced:
        assert "usd" not in item["amount"], "an unknown price must not be expressible as a number"
        assert item["amount"]["reason"]
    assert report["complete"] is False
    assert report["total"]["isLowerBound"] is True


def test_the_comparison_carries_its_own_counter_argument() -> None:
    # The warm-pool case is two orders of magnitude, and quoting only the ratio is how
    # a design that churns every few seconds looks affordable. The break-even is the
    # number a scheduler actually needs.
    _, stdout, _ = invoke(["cost", "--compare", "--hold-sec", "86400", "--json"])
    comparison = json.loads(stdout)["data"]["comparison"]
    assert float(comparison["ratio"]) > 1
    assert comparison["breakEvenSeconds"] > 0
    assert "avoid churn" in comparison["render"]


# -- parity and the surface --------------------------------------------------


def test_the_whole_surface_is_callable_without_a_shell(
    binary: pathlib.Path, infra_env: None, seam: Recorder
) -> None:
    # Pattern 5. `dispatch` returns a code and takes its argv, so anything a consumer
    # can do by shelling out they can do by importing — which is also what makes every
    # test in this file possible without a subprocess.
    assert cli.dispatch(["manifest", "--quiet"], out=cli.Output(stdout=io.StringIO())) == 0
    assert callable(cli.main)


def test_a_parse_error_still_honors_the_requested_format() -> None:
    # A parse failure never reaches a handler, so the error path reads the format off
    # the raw tokens. An agent that asked for JSON must get JSON even when what it
    # gets is an argument error — otherwise its first unparseable response is the one
    # telling it what it did wrong.
    code, stdout, _ = invoke(["not-a-command", "--json"])
    assert code == int(cli.Exit.INVALID_ARG)
    envelope = json.loads(stdout)
    assert envelope["status"] == "error"
    assert envelope["code"] == "ERR_INVALID_ARG"


def test_the_default_baseline_is_the_platforms_own_not_the_cheapest(
    binary: pathlib.Path, infra_env: None, seam: Recorder
) -> None:
    # The plan's call, and the reason is a failure mode rather than a preference: a
    # baseline is also the floor of the burst range, so a CLI that quietly picked
    # 0.5 GB to save about three cents an hour would hand someone a sandbox that
    # OOM-kills a real test suite. Guest swap is absent, so there is no paging phase
    # to absorb it.
    from microvms_agentd.sizing import SIZE_CLASSES

    invoke(["run", binary.as_posix(), "--exec", "true"])
    assert seam.kwargs("build_image")["memory_mib"] == 2048
    assert min(size.baseline_mib for size in SIZE_CLASSES) == 512, "and 512 is what was declined"


def test_no_command_can_request_a_shell_token_or_shell_ingress(
    binary: pathlib.Path, infra_env: None, seam: Recorder
) -> None:
    # `CreateMicrovmShellAuthToken` is not an exec path despite the name: it needs a
    # SHELL_INGRESS connector, its documented flow is a console terminal, and AWS
    # recommends it disabled in production. The library leaves it out of the connector
    # enum entirely, and the CLI must not be the place it comes back — a `--shell`
    # convenience flag is exactly how it would.
    manifest = cli.build_manifest()
    for command in manifest["commands"]:
        names = {p["name"] for p in command["parameters"]}
        assert not names & {"--shell", "--shell-ingress", "--console"}, command["name"]

    invoke(["run", binary.as_posix(), "--exec", "true"])
    recorded = json.dumps(seam.calls, default=str)
    assert "SHELL_INGRESS" not in recorded
    assert "shell_auth" not in recorded.lower()
