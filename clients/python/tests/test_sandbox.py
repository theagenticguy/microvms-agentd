"""`Sandbox` against a fake control plane: the AWS call shapes and the state machine.

Every assertion here is a trap from `docs/PLATFORM.md` that cost a real
build-and-run cycle. None of them is reachable from the API's shape, so a unit test
against a fake client is the only cheap place to hold them — the alternative is
finding out 45 minutes into an image build.
"""

from __future__ import annotations

import io
import json
import pathlib
import zipfile
from typing import Any

import pytest

from microvms_agentd import Sandbox


class FakeMv:
    """The `lambda-microvms` calls `Sandbox` makes, recording every kwarg."""

    def __init__(self, states: list[str] | None = None) -> None:
        self.calls: list[tuple[str, dict[str, Any]]] = []
        self._states = iter(states or ["PENDING", "RUNNING"])
        self.image_states = iter(["CREATED"])

    def _record(self, name: str, kw: dict[str, Any]) -> None:
        self.calls.append((name, dict(kw)))

    def kwargs(self, name: str) -> dict[str, Any]:
        return next(kw for called, kw in self.calls if called == name)

    def create_microvm_image(self, **kw: Any) -> dict[str, Any]:
        self._record("create_microvm_image", kw)
        return {"imageIdentifier": "img-1", "state": "CREATING"}

    def get_microvm_image(self, **_: Any) -> dict[str, Any]:
        return {"state": next(self.image_states, "CREATED")}

    def run_microvm(self, **kw: Any) -> dict[str, Any]:
        self._record("run_microvm", kw)
        return {"microvmId": "mvm-1", "endpoint": "abc.example", "state": "PENDING"}

    def get_microvm(self, **_: Any) -> dict[str, Any]:
        return {"state": next(self._states, "RUNNING"), "endpoint": "abc.example"}

    def suspend_microvm(self, **kw: Any) -> None:
        self._record("suspend_microvm", kw)
        self._states = iter(["SUSPENDED"])

    def resume_microvm(self, **kw: Any) -> None:
        self._record("resume_microvm", kw)
        self._states = iter(["RUNNING"])

    def terminate_microvm(self, **kw: Any) -> None:
        self._record("terminate_microvm", kw)

    def list_microvm_image_versions(self, **_: Any) -> dict[str, Any]:
        return {"items": [{"imageVersion": "1"}]}

    def list_microvm_image_builds(self, **_: Any) -> dict[str, Any]:
        return {"items": [{"state": "PENDING"}]}

    def delete_microvm_image(self, **kw: Any) -> None:
        self._record("delete_microvm_image", kw)

    def delete_microvm_image_version(self, **kw: Any) -> None:
        self._record("delete_microvm_image_version", kw)

    def create_microvm_auth_token(self, **_: Any) -> dict[str, Any]:
        return {"authToken": {"X-aws-proxy-auth": "jwe"}}


class FakeS3:
    def __init__(self) -> None:
        self.puts: list[dict[str, Any]] = []

    def put_object(self, **kw: Any) -> None:
        self.puts.append(kw)


class FakeLogs:
    def __init__(self) -> None:
        self.deleted: list[str] = []

    def delete_log_group(self, **kw: Any) -> None:
        self.deleted.append(kw["logGroupName"])


@pytest.fixture
def binary(tmp_path: pathlib.Path) -> pathlib.Path:
    path = tmp_path / "agentd"
    path.write_bytes(b"\x7fELF-not-really")
    return path


class FakeClock:
    """A hand-advanced monotonic clock, so a 600-second window costs no wall time."""

    def __init__(self) -> None:
        self.now = 0.0

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


def make(
    states: list[str] | None = None, *, clock: FakeClock | None = None
) -> tuple[Sandbox, FakeMv, FakeS3, FakeLogs]:
    mv, s3, logs = FakeMv(states), FakeS3(), FakeLogs()
    # Zero poll interval: the fake answers instantly, so the real 5s gap would be
    # 5s of pure sleep per test.
    box = Sandbox(
        region="us-east-1",
        microvm_client=mv,
        s3_client=s3,
        logs_client=logs,
        poll_interval_sec=0,
        **({"clock": clock} if clock is not None else {}),
    )
    return box, mv, s3, logs


def test_the_client_token_is_never_a_pure_function_of_content(binary) -> None:
    # It is a *permanent* idempotency key. A token derived only from the name or the
    # artifact bytes replays forever: delete the image, rebuild the same bytes, and
    # the service replays the original create as a no-op, wedging an image that then
    # cannot be deleted at all. Two were stuck that way for ~15 hours.
    box, mv, _, _ = make()
    box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    first = mv.kwargs("create_microvm_image")["clientToken"]

    box2, mv2, _, _ = make()
    box2.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    second = mv2.kwargs("create_microvm_image")["clientToken"]

    assert first != second, "same name and same bytes must still mint different tokens"


def test_the_agent_token_travels_in_the_run_hook_payload(binary) -> None:
    # Delivered at launch rather than baked into the shared snapshot. Safe because
    # the platform forwards no external traffic until the run hook returns 200.
    box, mv, _, _ = make()
    box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    box.run(execution_role_arn="arn:exec", agent_token="secret-token")

    payload = json.loads(mv.kwargs("run_microvm")["runHookPayload"])
    assert payload == {"agent_token": "secret-token"}


def test_the_network_connector_is_an_arn_not_a_bare_name(binary) -> None:
    # The literal "ALL_INGRESS" is rejected with "Malformed network connector ARN".
    box, mv, _, _ = make()
    box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    box.run(execution_role_arn="arn:exec")

    assert mv.kwargs("run_microvm")["ingressNetworkConnectors"] == [
        "arn:aws:lambda:us-east-1:aws:network-connector:aws-network-connector:ALL_INGRESS"
    ]


def test_egress_is_omitted_unless_asked_for(binary) -> None:
    # A daemon that needs no outbound network is one less thing a task workload can
    # reach.
    box, mv, _, _ = make()
    box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    box.run(execution_role_arn="arn:exec")
    assert "egressNetworkConnectors" not in mv.kwargs("run_microvm")


def test_the_session_gets_both_proxy_headers(binary) -> None:
    box, _, _, _ = make()
    box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    session = box.run(execution_role_arn="arn:exec")
    headers = session.transport.headers(token=None)
    assert headers == {"X-aws-proxy-auth": "jwe", "X-aws-proxy-port": "9000"}
    assert session.endpoint == "https://abc.example"


def test_a_terminal_state_before_running_fails_fast_with_the_state_reason() -> None:
    # A VM that reaches a terminal state before RUNNING died during startup, which
    # for a hook-serving daemon almost always means a lifecycle hook failed. Polling
    # through it wastes minutes and then reports a connection error that hides the
    # cause — and by then the VM is gone, so stateReason is the only evidence left.
    class DeadMv(FakeMv):
        def get_microvm(self, **_: Any) -> dict[str, Any]:
            return {
                "state": "TERMINATED",
                "stateReason": "Run lifecycle hook returned HTTP status 400.",
            }

    box = Sandbox(region="us-east-1", microvm_client=DeadMv(), poll_interval_sec=0)
    with pytest.raises(RuntimeError, match="HTTP status 400"):
        box.run(image_identifier="img-1", execution_role_arn="arn:exec", ready_timeout_sec=5)


def test_a_stalled_build_is_named_rather_than_waited_out(binary) -> None:
    # CREATING covers both "building" and "the service never scheduled this". The
    # second is the clientToken replay signature, and probing the build list turns a
    # 45-minute silent wait into an actionable failure.
    box, mv, _, _ = make()
    mv.image_states = iter(["CREATING"] * 100)
    with pytest.raises(RuntimeError, match="clientToken replay signature"):
        # Zero grace so the probe fires immediately rather than after four minutes.
        import microvms_agentd.sandbox as sandbox_mod

        original = sandbox_mod.STALL_GRACE_SEC
        sandbox_mod.STALL_GRACE_SEC = -1
        try:
            box.build_image(
                name="t-1",
                binary=binary,
                bucket="b",
                build_role_arn="arn:build",
                build_timeout_sec=1,
            )
        finally:
            sandbox_mod.STALL_GRACE_SEC = original


def test_suspend_and_resume_keep_the_same_session(binary) -> None:
    # No token re-delivery and no re-bootstrap: measured 2026-08-05, the in-memory
    # token, filesystem, exec records, and running processes all survive.
    box, mv, _, _ = make()
    box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    session = box.run(execution_role_arn="arn:exec")

    assert box.suspend() == "SUSPENDED"
    assert box.resume() is session, "the same Session, still holding the same agent token"
    assert "resume_microvm" in [name for name, _ in mv.calls]


def test_teardown_deletes_the_build_log_group_separately(binary) -> None:
    # The service creates `/aws/lambda-microvms/<image-name>` itself, so Terraform
    # never owns it and `destroy` leaves it behind. Six accumulated before anyone
    # noticed.
    box, mv, _, logs = make()
    box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    box.run(execution_role_arn="arn:exec")
    box.terminate(delete_image=True, delete_log_group=True)

    assert logs.deleted == ["/aws/lambda-microvms/t-1"]
    assert mv.kwargs("terminate_microvm") == {"microvmIdentifier": "mvm-1"}
    assert mv.kwargs("delete_microvm_image") == {"imageIdentifier": "img-1"}


def test_teardown_never_raises_because_it_runs_in_a_finally(binary) -> None:
    # An exception here would replace the real failure with a teardown failure, and
    # the real failure is the one worth reading.
    class HostileMv(FakeMv):
        def terminate_microvm(self, **_: Any) -> None:
            raise RuntimeError("throttled")

        def delete_microvm_image(self, **_: Any) -> None:
            raise RuntimeError("image is in CREATING")

    box = Sandbox(
        region="us-east-1", microvm_client=HostileMv(), logs_client=FakeLogs(), poll_interval_sec=0
    )
    box.microvm_id = "mvm-1"
    box.terminate(delete_image=False, delete_log_group=False)  # must not raise


def test_the_default_dockerfile_holds_the_deployment_invariant() -> None:
    # `ENTRYPOINT []` plus `CMD ["/agentd"]` is what guarantees no task workload runs
    # before the run hook lands, and what makes an omitted cwd inherit the WORKDIR.
    from microvms_agentd import default_dockerfile

    text = default_dockerfile(port=9000, workdir="/opt/baked-workdir")
    assert "ENTRYPOINT []" in text
    assert 'CMD ["/agentd"]' in text
    assert "WORKDIR /opt/baked-workdir" in text
    assert "ENV AGENTD_PORT=9000" in text


def test_the_artifact_zip_marks_the_binary_executable(binary) -> None:
    # A build that copies a non-executable binary produces an image whose CMD fails,
    # and that surfaces as a run-hook timeout rather than anything about permissions.
    import io
    import zipfile

    from microvms_agentd import build_artifact

    with zipfile.ZipFile(io.BytesIO(build_artifact(binary))) as archive:
        assert sorted(archive.namelist()) == ["Dockerfile", "agentd"]
        mode = archive.getinfo("agentd").external_attr >> 16
        assert mode & 0o111, f"not executable: {mode:o}"


def test_the_image_build_hooks_are_enabled(binary) -> None:
    # `ready` and `validate` are image-*build* hooks, called before any instance
    # exists. A daemon that omits them fails the build rather than the run.
    box, mv, _, _ = make()
    box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    hooks = mv.kwargs("create_microvm_image")["hooks"]
    assert hooks["microvmImageHooks"]["ready"] == "ENABLED"
    assert hooks["microvmImageHooks"]["validate"] == "ENABLED"
    assert hooks["microvmHooks"]["run"] == "ENABLED"
    assert hooks["port"] == 9000


# -- AC-1-1: the create token cannot be supplied at all ----------------------


def test_no_call_can_supply_its_own_idempotency_token(binary) -> None:
    # The stronger half of the clientToken trap, and the one a default cannot reach.
    # A per-attempt nonce protects the caller who passes nothing; it abandons the
    # caller who passes `client_token=<content digest>`, which is precisely the value
    # that wedges an image permanently. So the parameter is gone rather than defaulted
    # — the mistake is unwriteable, not merely discouraged.
    box, _, _, _ = make()
    with pytest.raises(TypeError, match="client_token"):
        box.build_image(
            name="t-1",
            binary=binary,
            bucket="b",
            build_role_arn="arn:build",
            client_token="create-t-1",  # type: ignore[call-arg]
        )
    with pytest.raises(TypeError, match="client_token"):
        box.run(
            image_identifier="img-1",
            execution_role_arn="arn:exec",
            client_token="run-t-1",  # type: ignore[call-arg]
        )


def test_the_token_scope_label_rides_next_to_the_nonce_not_instead_of_it(binary) -> None:
    # `token_scope` exists so CloudTrail is readable, and it is the shape that makes
    # the label safe: it lands *beside* the random suffix rather than replacing it, so
    # two attempts sharing a scope still mint different tokens.
    box, mv, _, _ = make()
    box.build_image(
        name="t-1", binary=binary, bucket="b", build_role_arn="arn:build", token_scope="run-42"
    )
    box2, mv2, _, _ = make()
    box2.build_image(
        name="t-1", binary=binary, bucket="b", build_role_arn="arn:build", token_scope="run-42"
    )

    first = mv.kwargs("create_microvm_image")["clientToken"]
    second = mv2.kwargs("create_microvm_image")["clientToken"]
    assert "run-42" in first and "run-42" in second, "the label must survive into the token"
    assert first != second, "a shared scope must not make two attempts share a token"


# -- AC-1-3: the capability field takes the one value AWS accepts ------------


def test_guest_identity_repair_is_an_intent_flag_not_a_capability_list(binary) -> None:
    # "ALL" is the only value the 2025-09-09 API accepts, and there is no way to ask
    # for CAP_SYS_ADMIN alone. A `list[str]` therefore lets a caller write
    # ["CAP_SYS_ADMIN"] — the natural-looking request AWS rejects only *after* the
    # artifact upload, one build cycle later. A boolean naming the intent has no wrong
    # value to pass.
    box, mv, _, _ = make()
    box.build_image(
        name="t-1",
        binary=binary,
        bucket="b",
        build_role_arn="arn:build",
        repair_guest_identity=True,
    )
    assert mv.kwargs("create_microvm_image")["additionalOsCapabilities"] == ["ALL"]

    box2, _, _, _ = make()
    with pytest.raises(TypeError, match="os_capabilities"):
        box2.build_image(
            name="t-2",
            binary=binary,
            bucket="b",
            build_role_arn="arn:build",
            os_capabilities=["CAP_SYS_ADMIN"],  # type: ignore[call-arg]
        )


def test_the_guest_is_not_widened_for_a_caller_who_did_not_ask(binary) -> None:
    # A caller who needs no hostname or boot_id repair should leave the guest narrow
    # rather than widen it for nothing, so the field is omitted rather than sent empty.
    box, mv, _, _ = make()
    box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    assert "additionalOsCapabilities" not in mv.kwargs("create_microvm_image")


# -- AC-1-4 / AC-1-5: the log group prefix, and an empty one ----------------


def test_the_build_log_group_uses_the_measured_prefix(binary) -> None:
    # `/aws/lambda-microvms/<name>`, not `/aws/lambda/microvms/*`. Asserted against
    # the measured literal rather than against the module constant, because a test
    # that reads the same constant the code writes cannot notice the prefix changing.
    box, _, _, _ = make()
    image = box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    assert image.build_log_group == "/aws/lambda-microvms/t-1"


def test_a_failed_build_with_no_logs_names_the_iam_prefix(binary) -> None:
    # Measured 2026-08-05: a build role granted the plausible-but-wrong
    # `/aws/lambda/microvms/*` produces builds that write no logs at all, and every
    # failure then reads `reason=unknown`. Forwarding `stateReason` verbatim makes a
    # discarded-log-policy indistinguishable from a silent service, and sends the
    # reader to investigate AWS instead of their own IAM.
    class FailingMv(FakeMv):
        def get_microvm_image(self, **_: Any) -> dict[str, Any]:
            return {"state": "CREATE_FAILED", "stateReason": None}

    class EmptyLogs(FakeLogs):
        def describe_log_streams(self, **_: Any) -> dict[str, Any]:
            return {"logStreams": [{"logStreamName": "s", "lastEventTimestamp": None}]}

    box = Sandbox(
        region="us-east-1",
        microvm_client=FailingMv(),
        s3_client=FakeS3(),
        logs_client=EmptyLogs(),
        poll_interval_sec=0,
    )
    with pytest.raises(RuntimeError) as caught:
        box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")

    message = str(caught.value)
    assert "/aws/lambda-microvms" in message, "the failure must name the prefix the role needs"
    assert "no events" in message
    assert "/aws/lambda/microvms/*" in message, "and the wrong spelling it is confused with"


def test_a_logs_client_that_throws_reads_as_unknown_not_as_empty(binary) -> None:
    # Claiming a misconfigured IAM prefix on the strength of a throttled API call
    # would send the reader after the wrong cause, which is the exact failure this
    # path exists to prevent. Unknown is not empty.
    class FailingMv(FakeMv):
        def get_microvm_image(self, **_: Any) -> dict[str, Any]:
            return {"state": "CREATE_FAILED", "stateReason": "build step 3 exited 1"}

    class ThrottledLogs(FakeLogs):
        def describe_log_streams(self, **_: Any) -> dict[str, Any]:
            raise RuntimeError("ThrottlingException")

    box = Sandbox(
        region="us-east-1",
        microvm_client=FailingMv(),
        s3_client=FakeS3(),
        logs_client=ThrottledLogs(),
        poll_interval_sec=0,
    )
    with pytest.raises(RuntimeError) as caught:
        box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")

    message = str(caught.value)
    assert "build step 3 exited 1" in message
    assert "no events" not in message, "a throttle must not be reported as an empty log group"


# -- AC-1-6: WORKDIR inheritance, and the FROM that must agree --------------


def test_inheriting_a_workdir_no_base_declares_is_rejected_before_any_upload(binary) -> None:
    # Measured 2026-08-05: `al2023-minimal`, `python:3.12-slim`, and `node:20-slim` all
    # leave `WorkingDir` empty, so "inherit the image WORKDIR" inherits `/` and every
    # relative path resolves somewhere the caller did not mean. Rejected rather than
    # warned because the symptom appears in the *guest*, one build cycle later, as
    # commands running in the wrong directory rather than as anything about WORKDIR.
    box, mv, s3, _ = make()
    with pytest.raises(ValueError, match="declares no WorkingDir"):
        box.build_image(
            name="t-1",
            binary=binary,
            bucket="b",
            build_role_arn="arn:build",
            inherit_workdir=True,
        )
    assert s3.puts == [], "rejected before the upload, not after"
    assert mv.calls == []


def test_inheritance_is_accepted_once_a_dockerfile_bakes_a_workdir(binary) -> None:
    # The guard is about there being nothing to inherit, not about the flag itself, so
    # a Dockerfile that sets WORKDIR satisfies it.
    from microvms_agentd import default_dockerfile

    box, mv, _, _ = make()
    box.build_image(
        name="t-1",
        binary=binary,
        bucket="b",
        build_role_arn="arn:build",
        dockerfile=default_dockerfile(workdir="/opt/work"),
        inherit_workdir=True,
    )
    assert "create_microvm_image" in [name for name, _ in mv.calls]


def test_the_default_dockerfiles_from_cannot_disagree_with_the_base_image_arn() -> None:
    # These two used to be able to drift: `DEFAULT_BASE_IMAGE` was `al2023-1` while
    # `default_dockerfile` hardcoded `amazonlinux:2023-minimal` in its FROM, so
    # changing either left the other pointing somewhere else. The build runs the
    # Dockerfile *on top of* the base named in `baseImageArn`, and a mismatch builds
    # against a base none of the measured platform behavior describes.
    from microvms_agentd import default_dockerfile
    from microvms_agentd.sandbox import BASE_IMAGES, DEFAULT_BASE_IMAGE, dockerfile_from_ref

    ref = dockerfile_from_ref(default_dockerfile())
    assert ref == BASE_IMAGES[DEFAULT_BASE_IMAGE].docker_ref
    assert ref == "public.ecr.aws/amazonlinux/amazonlinux:2023-minimal"


def test_a_dockerfile_whose_from_contradicts_the_base_image_is_rejected(binary) -> None:
    box, _, s3, _ = make()
    with pytest.raises(ValueError, match="must agree"):
        box.build_image(
            name="t-1",
            binary=binary,
            bucket="b",
            build_role_arn="arn:build",
            dockerfile='FROM docker.io/library/ubuntu:24.04\nCMD ["/agentd"]\n',
        )
    assert s3.puts == [], "rejected before the upload"


def test_an_unknown_base_image_name_is_rejected_rather_than_guessed(binary) -> None:
    # The name alone does not say what FROM pairs with it or what WORKDIR it declares,
    # and guessing either is how the two fell out of step before.
    box, _, _, _ = make()
    with pytest.raises(ValueError, match="unknown base image"):
        box.build_image(
            name="t-1",
            binary=binary,
            bucket="b",
            build_role_arn="arn:build",
            base_image="al2023-99",
        )


# -- AC-1-x: the size class reaches the request ------------------------------


def test_the_requested_baseline_is_the_one_the_create_call_sends(binary) -> None:
    # Sent verbatim rather than translated to the peak: `minimumMemoryInMiB` selects a
    # class and billing follows the baseline requested, so sending 8192 to get an 8 GB
    # guest would quadruple the bill for the same VM.
    box, mv, _, _ = make()
    image = box.build_image(
        name="t-1", binary=binary, bucket="b", build_role_arn="arn:build", memory_mib=512
    )
    assert mv.kwargs("create_microvm_image")["resources"] == [{"minimumMemoryInMiB": 512}]
    assert image.size.baseline_mib == 512
    assert image.size.peak_mib == 2048, "the class the caller actually got"


def test_the_default_baseline_is_the_platforms_own(binary) -> None:
    box, mv, _, _ = make()
    image = box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    assert mv.kwargs("create_microvm_image")["resources"] == [{"minimumMemoryInMiB": 2048}]
    assert image.size.describe().startswith("2 GB / 1 vCPU baseline")


def test_an_off_table_baseline_never_reaches_aws(binary) -> None:
    # Rejected locally, before the upload: what the service does with 1500 is
    # undocumented and unmeasured, and the two plausible readings differ in both the
    # memory the guest gets and the rate it is billed at.
    box, mv, s3, _ = make()
    with pytest.raises(ValueError, match="not a documented size class baseline"):
        box.build_image(
            name="t-1", binary=binary, bucket="b", build_role_arn="arn:build", memory_mib=1500
        )
    assert s3.puts == []
    assert mv.calls == []


# -- AC-2-1 / AC-3-4: no connector string, and no shell path ----------------


def test_the_connector_surface_names_no_free_form_string() -> None:
    # A closed enum rather than a `str` parameter. The API rejects the bare name with
    # "Malformed network connector ARN", so a free-form parameter invites the one value
    # that reads most natural and fails.
    from microvms_agentd import NetworkConnector
    from microvms_agentd.sandbox import connector_arn

    assert {c.value for c in NetworkConnector} == {"ALL_INGRESS", "INTERNET_EGRESS"}
    assert connector_arn("eu-west-1", NetworkConnector.INTERNET_EGRESS) == (
        "arn:aws:lambda:eu-west-1:aws:network-connector:aws-network-connector:INTERNET_EGRESS"
    )
    with pytest.raises(ValueError):
        NetworkConnector("ALL_INGRESS_TYPO")


def test_the_full_lifecycle_never_mints_a_shell_token_or_asks_for_shell_ingress(
    binary,
) -> None:
    # `CreateMicrovmShellAuthToken` exists and is not an exec path: it needs a
    # SHELL_INGRESS connector, its documented flow is `ctr task exec` through a console
    # terminal, and AWS scopes it to debugging while recommending it be disabled in
    # production. The name suggests a programmatic path it is not, and this client
    # exists precisely because no such path exists.
    #
    # Two assertions because absence-of-call alone is a weak guard: a client could
    # request the connector at launch without minting a token, which would widen the VM
    # for a capability nothing uses.
    from microvms_agentd import NetworkConnector

    box, mv, _, _ = make()
    box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    session = box.run(execution_role_arn="arn:exec", egress=True)
    session.transport.headers()
    box.suspend()
    box.resume()
    box.terminate(delete_image=True, delete_log_group=True)

    called = [name for name, _ in mv.calls]
    assert not any("shell" in name.lower() for name in called), called
    assert "SHELL_INGRESS" not in {c.value for c in NetworkConnector}
    for _, kwargs in mv.calls:
        assert "SHELL_INGRESS" not in json.dumps(kwargs, default=str)


# -- AC-2-3: the agent token is not in the artifact -------------------------


def test_the_agent_token_never_appears_in_the_uploaded_artifact(binary) -> None:
    # A scan of what actually went to S3, not an assertion that runHookPayload is
    # present. Baking the token into the Dockerfile as an ENV would still populate
    # runHookPayload and still pass the weaker check, while putting a per-VM secret
    # into a snapshot every sibling VM shares.
    #
    # Every member is *decompressed* before scanning, and that is the load-bearing part:
    # `build_artifact` writes with ZIP_DEFLATED, so a plaintext token in the Dockerfile
    # does not appear as plaintext in the archive bytes. Scanning the raw upload looks
    # like a strict guard and silently cannot fail — which is how this test was written
    # the first time.
    token = "unmistakable-agent-token-e3b0c442"
    box, mv, s3, _ = make()
    box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    box.run(execution_role_arn="arn:exec", agent_token=token)

    assert s3.puts, "the artifact was uploaded"
    for put in s3.puts:
        with zipfile.ZipFile(io.BytesIO(put["Body"])) as archive:
            for name in archive.namelist():
                content = archive.read(name)
                assert token.encode() not in content, f"the token reached {name}"
    assert token in mv.kwargs("run_microvm")["runHookPayload"], "and it did reach the VM"


# -- AC-4-3: a resume the idlePolicy already made impossible ----------------


def test_a_resume_of_an_already_terminated_vm_fails_fast_with_the_state_reason(
    binary,
) -> None:
    # The defect this closes: `resume` waited only for RUNNING, so a VM the idlePolicy
    # terminated during suspension burned the full 300s and then reported "never
    # reached RUNNING" — a timeout message hiding a cause the service had already
    # stated. That is the same cause-hiding failure `_wait_for_running` was written to
    # prevent, reintroduced on the resume path.
    class ReapedMv(FakeMv):
        """Suspends normally, then reports TERMINATED forever, as the idlePolicy does."""

        def resume_microvm(self, **kw: Any) -> None:
            self._record("resume_microvm", kw)
            self._states = iter([])  # falls through to the default below

        def get_microvm(self, **_: Any) -> dict[str, Any]:
            state = next(self._states, self.after_resume)
            return {"state": state, "stateReason": self.reason, "endpoint": "abc.example"}

        after_resume = "RUNNING"
        reason: str | None = None

    mv = ReapedMv()
    box = Sandbox(
        region="us-east-1",
        microvm_client=mv,
        s3_client=FakeS3(),
        logs_client=FakeLogs(),
        poll_interval_sec=0,
    )
    box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    box.run(execution_role_arn="arn:exec", suspended_sec=600)
    box.suspend()

    mv.after_resume = "TERMINATED"
    mv.reason = "MicroVM exceeded its suspended duration."
    # A generous timeout on purpose: the guard is that this returns on the *state*
    # rather than on the clock, so a 3600s bound that still returns immediately is the
    # assertion. A short timeout would pass even with the branch removed.
    with pytest.raises(RuntimeError) as caught:
        box.resume(timeout=3600.0)

    message = str(caught.value)
    assert "TERMINATED" in message
    assert "exceeded its suspended duration" in message, "stateReason is the only evidence left"
    assert "600s" in message, "and the window that closed must be named"


def test_a_resume_past_the_launch_time_window_is_rejected_without_calling_aws(
    binary,
) -> None:
    # The window came from our own RunMicrovm request and `GetMicrovm` does not return
    # it, so the client is the only party that can name the number. Checked locally
    # because the answer is already known — calling first would report whatever the
    # service says about a terminated id, which is not the same as "the window you set
    # at launch closed".
    clock = FakeClock()
    box, mv, _, _ = make(clock=clock)
    box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    box.run(execution_role_arn="arn:exec", suspended_sec=300)
    box.suspend()

    clock.advance(301)
    with pytest.raises(RuntimeError) as caught:
        box.resume()

    message = str(caught.value)
    assert "301s" in message, "the elapsed suspension"
    assert "300s" in message, "and the window it passed"
    assert "idlePolicy" in message
    assert "resume_microvm" not in [name for name, _ in mv.calls], "no call was made"


def test_a_resume_inside_the_window_still_works(binary) -> None:
    # The guard must reject a closed window without rejecting an open one, which is
    # the half a naive "always check" implementation gets wrong.
    clock = FakeClock()
    box, mv, _, _ = make(clock=clock)
    box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    session = box.run(execution_role_arn="arn:exec", suspended_sec=300)
    box.suspend()

    clock.advance(299)
    assert box.resume() is session
    assert "resume_microvm" in [name for name, _ in mv.calls]


def test_a_resume_of_an_already_running_vm_is_not_judged_by_a_spent_window(binary) -> None:
    # A successful resume clears the stamp, and this is the case that needs it. `suspend`
    # re-stamps on every cycle, so a suspend/resume loop hides a missing clear entirely;
    # what does not is a second `resume` with no suspend between — an idempotent retry,
    # or a caller who is not tracking state. Against a stale stamp the window check
    # measures time the VM spent *running* and rejects a healthy VM, reporting that the
    # idlePolicy terminated something that is sitting in RUNNING.
    clock = FakeClock()
    box, _, _, _ = make(clock=clock)
    box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    session = box.run(execution_role_arn="arn:exec", suspended_sec=300)

    box.suspend()
    clock.advance(100)
    assert box.resume() is session

    # Well past the window, but all of it spent RUNNING rather than suspended.
    clock.advance(5_000)
    assert box.resume() is session, "a running VM must not be refused by a spent window"


def test_a_warm_pool_can_cycle_indefinitely(binary) -> None:
    # The economic case for suspension is a pool that cycles many times. Each cycle's
    # window is its own, so accumulating elapsed time across cycles would make a pool
    # stop working after enough of them even though no single suspension came close.
    clock = FakeClock()
    box, _, _, _ = make(clock=clock)
    box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    session = box.run(execution_role_arn="arn:exec", suspended_sec=300)

    for _ in range(6):
        box.suspend()
        clock.advance(280)
        assert box.resume() is session
        clock.advance(280)


def test_suspend_reports_a_dying_vm_rather_than_raising_through_a_teardown(binary) -> None:
    # Why the fail-fast set is a parameter rather than always applied. A VM that dies
    # while suspending passes *through* TERMINATING, and `suspend` is typically called
    # on the way out — so raising there replaces whatever the caller was actually
    # dealing with. The state is the answer, and TERMINATED is a legitimate one to
    # return.
    #
    # TERMINATING rather than TERMINATED because the wanted set is checked first: with
    # TERMINATED the call returns before the fail-fast branch is ever reached, so this
    # is the only sequence that can observe the parameter's default at all.
    class DyingMv(FakeMv):
        def suspend_microvm(self, **kw: Any) -> None:
            self._record("suspend_microvm", kw)
            self._states = iter(["TERMINATING", "TERMINATING", "TERMINATED"])

    box = Sandbox(
        region="us-east-1", microvm_client=DyingMv(), s3_client=FakeS3(), poll_interval_sec=0
    )
    box.build_image(name="t-1", binary=binary, bucket="b", build_role_arn="arn:build")
    box.run(execution_role_arn="arn:exec")
    assert box.suspend() == "TERMINATED"
