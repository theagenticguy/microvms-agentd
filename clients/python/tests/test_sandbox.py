"""`Sandbox` against a fake control plane: the AWS call shapes and the state machine.

Every assertion here is a trap from `docs/PLATFORM.md` that cost a real
build-and-run cycle. None of them is reachable from the API's shape, so a unit test
against a fake client is the only cheap place to hold them — the alternative is
finding out 45 minutes into an image build.
"""

from __future__ import annotations

import json
import pathlib
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


def make(states: list[str] | None = None) -> tuple[Sandbox, FakeMv, FakeS3, FakeLogs]:
    mv, s3, logs = FakeMv(states), FakeS3(), FakeLogs()
    # Zero poll interval: the fake answers instantly, so the real 5s gap would be
    # 5s of pure sleep per test.
    box = Sandbox(
        region="us-east-1",
        microvm_client=mv,
        s3_client=s3,
        logs_client=logs,
        poll_interval_sec=0,
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
