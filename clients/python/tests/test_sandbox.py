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

import microvms_agentd.sandbox as mvsandbox
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
        # `buildState`, as the service model's `MicrovmImageBuildSummary` spells it —
        # that shape has no `state` member at all. This fake said `state` for as long
        # as the client read `state`, so the two agreed with each other and neither
        # agreed with AWS: the stall probe was dead against the real service while its
        # test passed. That is precisely the failure the spec's "a test may not assert
        # against a fake that shares the client's own assumptions" rule names, and the
        # model is the independent authority that broke the tie.
        return {"items": [{"buildState": "PENDING"}]}

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


# -- constraints the service model states ------------------------------------
#
# Every guard below closes a request the client used to be able to build and AWS
# would have rejected. They are local because botocore does not check them: reading
# botocore's `validate.py`, `VALIDATED_METADATA_ATTRS` is `{'required', 'min',
# 'document', 'union'}`, so `max`, `pattern`, and `enum` violations go to the wire
# and come back as a `ValidationException`. `scripts/check-model-drift` asserts the
# numbers here still match the shipped model; these tests assert the client acts on
# them.


def test_an_oversized_run_hook_payload_is_refused_before_the_launch() -> None:
    # The finding that started this: docs claimed a 16 KB ceiling against a real
    # ceiling of 4096, wrong by 4x in the direction that tells a caller four times as
    # much secret material fits as actually does. A caller who passes a JWT or a
    # signed credential blob as the agent token is who this catches, and the ceiling
    # is on the *serialized* payload, so the token has to be sized against the
    # JSON wrapper rather than against 4096 directly.
    box, mv, _, _ = make()
    over = "t" * mvsandbox.MAX_RUN_HOOK_PAYLOAD_BYTES
    with pytest.raises(ValueError, match="over the ceiling of 4096"):
        box.run(image_identifier="img-1", execution_role_arn="arn:exec", agent_token=over)
    assert "run_microvm" not in [name for name, _ in mv.calls], "must not reach the wire"


def test_a_payload_at_the_ceiling_is_accepted_because_the_bound_is_inclusive() -> None:
    # Bracketed from both sides against real AWS on 2026-08-07: 4096 passes the length
    # check, 4097 is rejected. An off-by-one here would refuse a payload the service
    # accepts, and a caller who had measured their own fit would not believe the error.
    box, mv, _, _ = make()
    wrapper = len(json.dumps({"agent_token": ""}))
    token = "t" * (mvsandbox.MAX_RUN_HOOK_PAYLOAD_BYTES - wrapper)
    box.run(image_identifier="img-1", execution_role_arn="arn:exec", agent_token=token)
    payload = mv.kwargs("run_microvm")["runHookPayload"]
    assert len(payload.encode()) == mvsandbox.MAX_RUN_HOOK_PAYLOAD_BYTES


def test_the_payload_ceiling_is_measured_in_bytes_not_characters() -> None:
    # `len()` and the serialized length disagree the moment a caller's token carries a
    # multi-byte character, and the service counts bytes. Checking `len()` would pass a
    # payload the service rejects — the exact shape of the bug this guard replaced.
    assert mvsandbox.require_payload_fits("a" * 4096) is not None
    with pytest.raises(ValueError, match="4097 bytes"):
        # 4095 characters, one of which serializes to two bytes.
        mvsandbox.require_payload_fits("a" * 4095 + "é" * 1)


@pytest.mark.parametrize(
    "name, signature",
    [
        ("", "at least 1 character"),
        ("n" * 65, "65 characters, over"),
        ("has.a.dot", "no dots and no slashes"),
        ("has/a/slash", "no dots and no slashes"),
        ("has a space", "does not match"),
    ],
)
def test_an_image_name_the_service_would_reject_is_refused_before_the_upload(
    binary, name: str, signature: str
) -> None:
    # The order matters here, which is why the length case has its own message: the
    # pattern message names dots and slashes, and reading that about a 70-character
    # name containing neither sends the reader looking for a character that is not
    # there. Refused before `put_object` because the create call — the one AWS would
    # reject — happens after the artifact is already uploaded.
    box, mv, s3, _ = make()
    with pytest.raises(ValueError, match=signature):
        box.build_image(name=name, binary=binary, bucket="b", build_role_arn="arn:build")
    assert s3.puts == [], "the artifact must not be uploaded for a name AWS will reject"
    assert mv.calls == []


def test_the_run_time_and_build_time_hook_ceilings_are_60x_apart(binary) -> None:
    # The trap this project exists to close, in miniature. A caller who picks one
    # timeout large enough for a build hook (say 300s) is inside the 3600s image-hook
    # ceiling and 5x over the 60s run-hook one — so the request passes on the family
    # they were thinking about and fails on the one they were not, after the upload,
    # reported as a constraint on a field they did not know had two ceilings.
    box, _, s3, _ = make()
    with pytest.raises(ValueError, match=r"microvmHooks\.runTimeoutInSeconds=300.*1\.\.60"):
        box.build_image(
            name="t-1",
            binary=binary,
            bucket="b",
            build_role_arn="arn:build",
            hooks=mvsandbox.default_hooks(9000, timeout=60, image_timeout=300)
            | {"microvmHooks": {"run": "ENABLED", "runTimeoutInSeconds": 300}},
        )
    assert s3.puts == []

    # And the other direction: 300s is fine on a build hook, so a client that applied
    # one ceiling to both families would refuse a legal request.
    hooks = mvsandbox.default_hooks(9000, timeout=30, image_timeout=300)
    assert hooks["microvmImageHooks"]["readyTimeoutInSeconds"] == 300
    assert hooks["microvmHooks"]["runTimeoutInSeconds"] == 30


def test_a_build_hook_timeout_over_its_own_ceiling_is_still_refused() -> None:
    # 3600 is the build family's ceiling, so this proves the guard is checking the
    # right bound per family rather than letting the larger one stand in for both.
    with pytest.raises(ValueError, match=r"readyTimeoutInSeconds=3601.*1\.\.3600"):
        mvsandbox.default_hooks(9000, timeout=30, image_timeout=3601)


def test_a_duration_over_eight_hours_is_refused_locally(binary) -> None:
    # 28800 seconds is the hard ceiling on any one VM's life. A longer session needs a
    # second VM, and the error says so — the service's own answer names a constraint
    # and leaves the caller to work out that no number will do.
    box, mv, _, _ = make()
    with pytest.raises(ValueError, match=r"outside the accepted range 1\.\.28800"):
        box.run(image_identifier="img-1", execution_role_arn="arn:exec", max_duration_sec=28_801)
    assert mv.calls == []


def test_a_non_arm_architecture_is_refused_because_the_enum_has_one_value(binary) -> None:
    # `Architecture` is exactly `['ARM_64']`, read out of the service model rather than
    # believed. The reason to refuse locally is the symptom: an x86 binary in the image
    # surfaces as a run-hook *timeout*, which says nothing about architecture.
    box, _, s3, _ = make()
    with pytest.raises(ValueError, match="ARM64-only"):
        box.build_image(
            name="t-1",
            binary=binary,
            bucket="b",
            build_role_arn="arn:build",
            architecture="X86_64",
        )
    assert s3.puts == []


def test_a_region_that_does_not_carry_microvms_is_refused_at_construction() -> None:
    # Measured 2026-08-07: an unsupported region answers AccessDeniedException with a
    # *null* message, indistinguishable from a real IAM denial — so a typo'd region
    # sends someone to audit a policy that is fine. Construction time rather than first
    # call, because the first call is where that evidence is destroyed. Nothing upstream
    # objects: botocore lists zero regions for this service, yet the client builds for
    # any region.
    with pytest.raises(ValueError, match="null message"):
        Sandbox(region="eu-central-1", microvm_client=FakeMv())
    # The override exists because AWS adds regions faster than the list is re-read, and
    # a client that refuses a region AWS just launched in is its own kind of wrong.
    assert (
        Sandbox(region="eu-central-1", microvm_client=FakeMv(), allow_unlisted_region=True).region
        == "eu-central-1"
    )


def test_eu_central_1_is_not_in_the_region_list() -> None:
    # It was, in `cli.MICROVM_REGIONS`, until 2026-08-07 — and it is one of the three
    # regions measured returning the null-message denial. Named explicitly rather than
    # left to the set comparison because a wrong *member* is the dangerous direction: a
    # missing region makes the client refuse something valid, an extra one hands the
    # caller the undiagnosable failure this whole guard exists to prevent.
    assert "eu-central-1" not in mvsandbox.MICROVM_REGIONS
    from microvms_agentd import cli

    assert cli.MICROVM_REGIONS is mvsandbox.MICROVM_REGIONS, "one list, not two copies"


def test_a_minted_client_token_fits_the_128_ceiling_for_the_worst_legal_input() -> None:
    # Found 2026-08-07 by the drift checker's *coverage* report, not by a failing
    # check: it named `RunMicrovmRequestClientTokenString` as a constraint nothing was
    # bound to. `run` defaults its token scope to the image identifier — a full ARN —
    # so an ap-northeast-1 ARN carrying a legal 64-character image name minted a
    # 142-character token against a 128 ceiling. botocore does not validate `max`, so
    # it would have reached the wire and failed the launch on a field the caller never
    # set. A test on a short name passes either way, which is why this one uses the
    # longest legal input rather than a typical one.
    worst = f"arn:aws:lambda:ap-northeast-1:123456789012:microvm-image:{'n' * 64}"
    for minted in (mvsandbox._create_token(worst), mvsandbox._run_token(worst)):
        assert len(minted) <= mvsandbox.MAX_CLIENT_TOKEN_LEN, minted

    # Truncating the label must not touch the nonce: a `clientToken` is a permanent
    # idempotency key, so two attempts sharing a truncated scope colliding would be
    # the wedge again — worse than the length error it was fixing.
    assert len({mvsandbox._run_token(worst) for _ in range(200)}) == 200


def test_the_stall_probe_reads_build_state_as_the_model_spells_it(binary) -> None:
    # The guard that was dead against live AWS while its test passed. The model's
    # `MicrovmImageBuildSummary` has no `state` member at all — only `buildState` — so
    # `b.get("state")` returned None for every real response and the all-PENDING test
    # could never be true. It was invisible because the fake also said `state`: the
    # fake shared the client's own misreading, which is exactly what the spec's "a test
    # may not assert against a fake that shares the client's own assumptions" rule
    # forbids. The service model was the independent authority that broke the tie.
    #
    # Asserted against a fake that answers *only* `buildState`, so a client that reads
    # `state` finds nothing and this test fails.
    class ModelShapedMv(FakeMv):
        def list_microvm_image_builds(self, **_: Any) -> dict[str, Any]:
            return {"items": [{"buildId": "b-1", "buildState": "PENDING"}]}

    box = Sandbox(
        region="us-east-1",
        microvm_client=ModelShapedMv(states=["PENDING"]),
        s3_client=FakeS3(),
        poll_interval_sec=0,
    )
    box._mv.image_states = iter(["CREATING"] * 100)
    original = mvsandbox.STALL_GRACE_SEC
    mvsandbox.STALL_GRACE_SEC = -1
    try:
        with pytest.raises(RuntimeError, match="clientToken replay signature"):
            box.build_image(
                name="t-1",
                binary=binary,
                bucket="b",
                build_role_arn="arn:build",
                build_timeout_sec=1,
            )
    finally:
        mvsandbox.STALL_GRACE_SEC = original


def test_every_pinned_constraint_is_covered_by_the_drift_checker() -> None:
    # The checker is only worth trusting if it reads the same constants the client
    # acts on. It imports `sandbox` directly rather than re-transcribing them, and this
    # asserts the names it depends on still exist — a renamed constant would otherwise
    # make the checker fail to import and the failure would read as a broken script
    # rather than as drift.
    for attr in (
        "MODEL_API_VERSION",
        "MAX_RUN_HOOK_PAYLOAD_BYTES",
        "MAX_IMAGE_NAME_LEN",
        "IMAGE_NAME_PATTERN",
        "MAX_DURATION_SEC",
        "MAX_MICROVM_HOOK_TIMEOUT_SEC",
        "MAX_IMAGE_HOOK_TIMEOUT_SEC",
        "MAX_HOOK_PORT",
        "MAX_CLIENT_TOKEN_LEN",
        "MAX_NETWORK_CONNECTORS",
        "MAX_RESOURCES",
        "ARCHITECTURES",
        "CAPABILITIES",
        "MODEL_IMAGE_READY_STATES",
        "MICROVM_REGIONS",
    ):
        assert hasattr(mvsandbox, attr), f"check-model-drift reads sandbox.{attr}"
