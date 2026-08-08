"""The AWS side: build an image, launch a MicroVM, hand back a `Session`.

Every function here is lifted from `conformance/run.py` and
`conformance/probe_suspend_resume.py`, which have been run against real AWS. The
traps are inlined as comments because each one cost a full build-and-run cycle to
find, and none of them is discoverable from the API shape.

boto3 is imported lazily so that `Session` — the part with no AWS in it — stays
importable and testable without credentials or the SDK.
"""

from __future__ import annotations

import contextlib
import io
import json
import re
import secrets
import time
import zipfile
from collections.abc import Callable
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any

from .session import Session
from .sizing import DEFAULT_BASELINE_MIB, SizeClass, size_class_for
from .transport import DEFAULT_AGENT_PORT

SERVICE = "lambda-microvms"

#: The `MicrovmImageState` values that mean "built and usable", as the model spells
#: them. `UPDATED` is here even though this client never calls `UpdateMicrovmImage`:
#: an image someone else updated is usable, and treating it as still-building is a
#: 45-minute wait on a state that will never change.
MODEL_IMAGE_READY_STATES = frozenset({"CREATED", "UPDATED"})

#: Spellings that are *not* in the `2025-09-09` `MicrovmImageState` enum. Kept
#: because the service has answered differently across API versions and a hard
#: equality check on one spelling is how a working build looks like a stalled one,
#: but held separately so `scripts/check-model-drift` can check the model-derived
#: set exactly instead of being told that two of the three values it cannot find are
#: fine. If a future model adds either, move it up.
_TOLERATED_IMAGE_READY_STATES = frozenset({"ACTIVE", "AVAILABLE"})

IMAGE_READY_STATES = MODEL_IMAGE_READY_STATES | _TOLERATED_IMAGE_READY_STATES

#: Terminal states. Reaching any of them *before* RUNNING means the VM died during
#: startup, which for a hook-serving daemon almost always means a lifecycle hook
#: failed — and `stateReason` is where the answer is.
TERMINAL_STATES = frozenset({"TERMINATED", "TERMINATING", "SUSPENDED", "SUSPENDING"})

#: The subset of `TERMINAL_STATES` from which nothing comes back. Separate from
#: `TERMINAL_STATES` because SUSPENDED is a death *before* RUNNING and an ordinary
#: waypoint on the resume path — a resume that failed fast on SUSPENDED would fail
#: on every resume, since that is the state the VM is in when the call is made.
DEAD_STATES = frozenset({"TERMINATED", "TERMINATING"})

DEFAULT_BASE_IMAGE = "al2023-1"
DEFAULT_HOOK_TIMEOUT_SEC = 30
DEFAULT_IMAGE_BUILD_TIMEOUT_SEC = 45 * 60
#: How long an image may sit in CREATING before we probe for the stalled-build
#: signature. Long enough that a genuinely slow build is not accused, short enough
#: that a wedged one does not burn the full 45 minutes in silence.
STALL_GRACE_SEC = 240
#: Gap between MicroVM state polls. A launch takes tens of seconds, so a tighter
#: interval only spends control-plane quota. Overridable per Sandbox so a test does
#: not have to sleep through it.
DEFAULT_POLL_INTERVAL_SEC = 5.0


#: Bits of randomness folded into every idempotency token. Eight bytes, so two
#: attempts one second apart do not collide even under a retry storm.
_TOKEN_NONCE_BYTES = 8


# ── what the service model states ───────────────────────────────────────────
#
# Every number and pattern below is transcribed from the botocore service model
# for `lambda-microvms`, and `scripts/check-model-drift` fails when any of them
# stops matching the shipped model. The model is a machine-readable statement of
# the service's own request validation; restating it by hand is how this project
# published a 16 KB `runHookPayload` ceiling against a real ceiling of 4096.
#
# These are checked locally because **botocore does not check them.** Measured
# 2026-08-07 by reading botocore's `validate.py`: `VALIDATED_METADATA_ATTRS` is
# `{'required', 'min', 'document', 'union'}`, so a `min` violation raises
# `ParamValidationError` before the wire, and `max`, `pattern`, and `enum`
# violations are serialized, sent, and answered with a `ValidationException`.
# Confirmed empirically for `max` (runHookPayload 4097, maximumDurationInSeconds
# 28801, ImageName 65 chars, NetworkConnectorList 11 items, clientToken 129
# chars), for `pattern` (ImageName "a b!"), and for `enum` (architecture
# X86_64, additionalOsCapabilities CAP_SYS_ADMIN) — every one reached the wire.
#
# So the guards below are load-bearing rather than belt-and-braces, and the
# obvious future simplification — "botocore validates the model already, delete
# these" — silently reopens all of them.
#
# `IdlePolicy.maxIdleDurationSeconds` is the counter-example and deliberately has
# no guard here: its constraint is `min: 60`, which botocore *does* enforce
# locally with a clear message. A second check would be redundant.

#: The model version every constraint here was read from.
MODEL_API_VERSION = "2025-09-09"

#: `RunMicrovmRequestRunHookPayloadString.max`. Inclusive: 4096 bytes passes the
#: length check and 4097 is rejected, bracketed 2026-08-07 by calling `RunMicrovm`
#: with a deliberately bogus `imageIdentifier` so nothing could be created or
#: billed. `docs/STRATEGY.md` and `docs/TRUST.md` claimed 16 KB, which is wrong by
#: 4x in the dangerous direction — it tells a caller four times as much secret
#: material fits as actually does.
MAX_RUN_HOOK_PAYLOAD_BYTES = 4096

#: `ImageName`: min 1, max 64, pattern `[a-zA-Z0-9-_]+`. No dots and no slashes,
#: which rules out the two separators a caller reaching for a namespaced name
#: writes first.
MAX_IMAGE_NAME_LEN = 64
IMAGE_NAME_PATTERN = r"[a-zA-Z0-9-_]+"
_IMAGE_NAME_RE = re.compile(rf"\A(?:{IMAGE_NAME_PATTERN})\Z")

#: `RunMicrovmRequestMaximumDurationInSecondsInteger`: min 1, max 28800 — eight
#: hours, and the hard ceiling on any single VM's life.
MAX_DURATION_SEC = 28800

#: `MicrovmHooks*TimeoutInSecondsInteger` (run, resume, suspend, terminate):
#: max 60. `MicrovmImageHooks*TimeoutInSecondsInteger` (ready, validate): max
#: 3600. The 60x gap follows from what each family is for — a build hook waits on
#: a Dockerfile, a run hook waits on a daemon that is already booted — and it is
#: the whole reason `default_hooks` takes two timeouts rather than one. A single
#: shared value large enough for a build (say 300) is rejected on the run family
#: only, after the artifact is uploaded.
MAX_MICROVM_HOOK_TIMEOUT_SEC = 60
MAX_IMAGE_HOOK_TIMEOUT_SEC = 3600

#: `HooksPortInteger`: min 1, max 65535.
MAX_HOOK_PORT = 65535

#: `Capability` enum is exactly this one value, which is why `build_image` takes a
#: `repair_guest_identity: bool` naming the intent rather than a capability list.
#: There is no way to ask for `CAP_SYS_ADMIN` alone.
CAPABILITIES = ("ALL",)

#: `Architecture` enum is exactly this one value: a MicroVM cannot be x86. Machine
#: checkable rather than folklore — the drift checker reads it out of the model.
ARCHITECTURES = ("ARM_64",)
DEFAULT_ARCHITECTURE = "ARM_64"

#: `NetworkConnectorList.max` and `ResourcesList.max`. Only one resources entry is
#: accepted, so "give the VM two memory floors" is not a thing that can be asked.
MAX_NETWORK_CONNECTORS = 10
MAX_RESOURCES = 1

#: Regions that answered `ListMicrovms` when this was written. Keeping this list
#: correct is the whole correctness condition, in both directions. A *missing*
#: region makes this client refuse a launch AWS would have accepted, which is the
#: safer direction and still wrong — `allow_unlisted_region=True` is the escape
#: hatch, and it costs the caller the diagnostic below. An *extra* region is worse:
#: measured 2026-08-07, a region that does not carry MicroVMs answers
#: `AccessDeniedException` with a null message field, which is indistinguishable
#: from a real IAM denial, so a typo'd region sends someone to audit a policy that
#: is fine.
#:
#: No botocore call answers the question, and the two that look like they might
#: disagree with each other: `endpoint_resolver.get_available_endpoints` returns an
#: empty list while `session.get_available_regions` returns all 34 Lambda regions,
#: because the model's `endpointPrefix` is `lambda`. Neither is this list, so
#: neither can stand in for it. `boto3.client(...)` also constructs happily for any
#: region and resolves to `https://lambda.<region>.amazonaws.com`.
#:
#: `eu-central-1` was in this list until 2026-08-07 and does *not* carry MicroVMs:
#: it was one of the three regions measured returning the null-message denial.
#:
#: This is the single definition. `pricing.py` imports it rather than restating it,
#: because the copy in `cli.py` had already drifted to include `eu-central-1` while
#: this one was correct, and two lists that must agree are one list that will not.
MICROVM_REGIONS = frozenset({"us-east-1", "us-east-2", "us-west-2", "eu-west-1", "ap-northeast-1"})

#: `Create/Run/UpdateMicrovmImageRequestClientTokenString.max`, all three 128.
MAX_CLIENT_TOKEN_LEN = 128

#: How much of the scope label survives into a token. Not a cosmetic cap. The tokens
#: are `<verb>-<scope>-<16 hex>`, and `run`'s scope defaults to the *image
#: identifier* — a full ARN. Found 2026-08-07 by the drift checker's coverage report
#: naming `RunMicrovmRequestClientTokenString` as unbound: an ap-northeast-1 ARN
#: carrying a legal 64-character image name mints a 142-character token, over the
#: 128 ceiling, and botocore does not check `max` so it would have gone to the wire
#: and failed the launch on a field the caller never set.
#:
#: The truncation is on the *label*, never the nonce, so a shortened scope cannot
#: make two attempts collide — which is the one property of these tokens that
#: matters, since a `clientToken` is a permanent idempotency key.
_MAX_TOKEN_SCOPE_LEN = 64


def _token(verb: str, scope: str) -> str:
    nonce = secrets.token_hex(_TOKEN_NONCE_BYTES)
    # Keep the tail rather than the head: an ARN's distinguishing part is the
    # resource name at the end, and every ARN in a region shares its prefix.
    label = scope[-_MAX_TOKEN_SCOPE_LEN:]
    token = f"{verb}-{label}-{nonce}"
    assert len(token) <= MAX_CLIENT_TOKEN_LEN, token
    return token


def require_valid_image_name(name: str) -> str:
    """Rejects an image name the service would reject, before the artifact upload.

    Order matters: the emptiness and length cases get their own messages because the
    pattern message ("no dots, no slashes") is actively misleading for a 70-character
    name that contains neither.
    """
    if not name:
        raise ValueError(
            "image name is empty, but ImageName requires at least 1 character "
            f"(service model {MODEL_API_VERSION})."
        )
    if len(name) > MAX_IMAGE_NAME_LEN:
        raise ValueError(
            f"image name is {len(name)} characters, over the ImageName ceiling of "
            f"{MAX_IMAGE_NAME_LEN} (service model {MODEL_API_VERSION}). Rejected here "
            "rather than by AWS, because the create call happens *after* the artifact "
            "upload — so the service's answer costs you the upload first."
        )
    if not _IMAGE_NAME_RE.fullmatch(name):
        raise ValueError(
            f"image name {name!r} does not match the ImageName pattern "
            f"{IMAGE_NAME_PATTERN!r} (service model {MODEL_API_VERSION}). Letters, "
            "digits, hyphen, and underscore only — no dots and no slashes, which are "
            "the two separators a namespaced name reaches for first."
        )
    return name


def require_payload_fits(payload: str) -> str:
    """Rejects a `runHookPayload` over the service ceiling, before `RunMicrovm`.

    Bytes rather than characters: the ceiling is on the serialized string, so a
    payload measured in `len()` passes while the same value with one multi-byte
    character in it does not. `json.dumps` escapes non-ASCII by default, which makes
    the two agree for the client's own token payload and disagree for a caller's.

    This is where a caller learns that a credential bundle does not fit. The
    alternative is a `ValidationException` from the control plane after the launch
    request is already in flight, naming a constraint rather than a remedy.
    """
    size = len(payload.encode("utf-8"))
    if size <= MAX_RUN_HOOK_PAYLOAD_BYTES:
        return payload
    raise ValueError(
        f"runHookPayload is {size} bytes, over the ceiling of "
        f"{MAX_RUN_HOOK_PAYLOAD_BYTES} (service model {MODEL_API_VERSION}, measured "
        f"inclusive 2026-08-07). This is the only per-VM secret channel the platform "
        f"offers: one bearer token fits, a cloud credential set does not. Note "
        f"docs/STRATEGY.md and docs/TRUST.md claimed 16 KB, which is wrong by 4x."
    )


def require_duration_in_range(seconds: int) -> int:
    """Rejects a `maximumDurationInSeconds` outside the service range."""
    if 1 <= seconds <= MAX_DURATION_SEC:
        return seconds
    raise ValueError(
        f"maximumDurationInSeconds={seconds} is outside the accepted range 1.."
        f"{MAX_DURATION_SEC} (service model {MODEL_API_VERSION}) — "
        f"{MAX_DURATION_SEC}s is eight hours, the hard ceiling on any one VM's life. "
        "A longer session needs a second VM, not a larger number."
    )


def require_hook_timeouts_in_range(hooks: dict[str, Any]) -> dict[str, Any]:
    """Rejects a hook block whose timeouts the service would reject.

    Two families with ceilings 60x apart, and confusing them is exactly the trap
    this validation exists to close: `run`/`resume`/`suspend`/`terminate` cap at
    60 seconds, `ready`/`validate` at 3600. A caller who picks one number large
    enough for a build hook passes image validation and fails on the run family —
    after the artifact upload, and reported as a constraint on a field they did not
    know had two different ceilings.
    """
    families = (
        ("microvmHooks", MAX_MICROVM_HOOK_TIMEOUT_SEC, ("run", "resume", "suspend", "terminate")),
        ("microvmImageHooks", MAX_IMAGE_HOOK_TIMEOUT_SEC, ("ready", "validate")),
    )
    for block_name, ceiling, hook_names in families:
        block = hooks.get(block_name) or {}
        for hook in hook_names:
            key = f"{hook}TimeoutInSeconds"
            if key not in block:
                continue
            value = block[key]
            if 1 <= value <= ceiling:
                continue
            other = (
                MAX_IMAGE_HOOK_TIMEOUT_SEC
                if ceiling == MAX_MICROVM_HOOK_TIMEOUT_SEC
                else MAX_MICROVM_HOOK_TIMEOUT_SEC
            )
            raise ValueError(
                f"{block_name}.{key}={value} is outside the accepted range 1..{ceiling} "
                f"(service model {MODEL_API_VERSION}). The two hook families have "
                f"ceilings 60x apart — {block_name} caps at {ceiling}s while the other "
                f"family caps at {other}s — because a build hook waits on a Dockerfile "
                f"and a run hook waits on a daemon that is already booted."
            )
    port = hooks.get("port")
    if port is not None and not 1 <= port <= MAX_HOOK_PORT:
        raise ValueError(
            f"hooks.port={port} is outside 1..{MAX_HOOK_PORT} (service model {MODEL_API_VERSION})."
        )
    return hooks


def require_supported_region(region: str, *, allow_unlisted: bool = False) -> str:
    """Rejects a region that does not carry MicroVMs, at construction time.

    Construction time rather than first call, because the first call is where the
    diagnostic is destroyed: measured 2026-08-07, an unsupported region answers
    `AccessDeniedException` with a *null* message, which reads as a genuine IAM
    denial and sends the reader to audit a policy that is fine. Nothing between the
    caller and that answer objects — botocore's endpoint resolver lists zero regions
    for `lambda-microvms`, and `boto3.client` happily builds one for any region.

    `allow_unlisted` exists because AWS adds regions faster than this list is
    re-read, and a client that refuses a region AWS has just launched in is its own
    kind of wrong. The override costs exactly the diagnostic above.
    """
    if allow_unlisted or region in MICROVM_REGIONS:
        return region
    known = ", ".join(sorted(MICROVM_REGIONS))
    raise ValueError(
        f"region {region!r} is not one this client has seen carry MicroVMs ({known}). "
        "Refused here because the first API call is where the evidence disappears: an "
        "unsupported region answers AccessDeniedException with a null message, which "
        "is indistinguishable from a real IAM denial (docs/PLATFORM.md, 'Calling an "
        "unpriced region returns AccessDeniedException with a null message'). If AWS "
        "has since launched MicroVMs here, pass allow_unlisted_region=True and add the "
        "region to MICROVM_REGIONS."
    )


def _create_token(scope: str) -> str:
    """An image-create idempotency token, unique per attempt by construction.

    A `clientToken` is a *permanent* idempotency key. Measured 2026-08-02, the
    expensive way: delete an image and recreate it from the same bytes under the same
    name with a token derived from that identity, and the service replays the original
    create as a no-op. The image sits in `CREATING` with its builds never scheduled,
    cannot be deleted (`CREATING` forbids it), and its only version cannot be dropped
    (it is the last one). Two images were wedged that way for ~15 hours.

    So there is no caller-supplied token parameter anywhere in this client, only a
    `token_scope` label that lands *next to* the nonce. That is the difference between
    a default a caller can override and a mistake a caller cannot write: the previous
    shape defaulted correctly and accepted `client_token=<content digest>`, which is
    precisely the value that wedges an image.
    """
    return _token("create", scope)


def _run_token(scope: str) -> str:
    """A run idempotency token, unique per attempt. Same rule as `_create_token`.

    Cheaper to get wrong than the image case — a replayed run returns the original
    MicroVM rather than wedging anything — but the failure is worse to read: a caller
    who asked for a second VM gets the first one's id back and two callers then drive
    the same guest.
    """
    return _token("run", scope)


class NetworkConnector(Enum):
    """The Lambda-managed connectors this client will name, and no others.

    A closed set rather than a string parameter, for two reasons. The API takes a
    fully-qualified ARN and rejects the bare name with "Malformed network connector
    ARN" (measured 2026-08-05), so a free-form parameter invites the one value that
    reads most natural and fails. And `SHELL_INGRESS` is deliberately absent: it
    exists in the API, but it gates `CreateMicrovmShellAuthToken`, whose documented
    flow is `ctr task exec` through a console terminal, scoped to debugging and
    recommended disabled in production. It is not a programmatic exec path despite
    the name, and this client's whole reason to exist is that no such path exists.
    Leaving it out of the enum is what makes requesting it unwriteable rather than
    merely discouraged.
    """

    ALL_INGRESS = "ALL_INGRESS"
    INTERNET_EGRESS = "INTERNET_EGRESS"


def connector_arn(region: str, connector: NetworkConnector = NetworkConnector.ALL_INGRESS) -> str:
    """The fully-qualified ARN for a managed connector in `region`.

    One interpolation for both directions. The previous shape derived the egress ARN
    by string-replacing `ALL_INGRESS` inside the ingress one, which produced a valid
    ARN only as long as the two names never became substrings of each other.
    """
    return f"arn:aws:lambda:{region}:aws:network-connector:aws-network-connector:{connector.value}"


def ingress_connector_arn(region: str) -> str:
    """The connector that lets the endpoint proxy reach the VM.

    Omitting egress is how you get a VM with no outbound network — which is the right
    default for a daemon that needs none.
    """
    return connector_arn(region, NetworkConnector.ALL_INGRESS)


@dataclass(frozen=True)
class BaseImage:
    """A base image, its Dockerfile `FROM`, and whether it declares a `WORKDIR`.

    All three in one object because the first two must agree and used to be able to
    disagree: `DEFAULT_BASE_IMAGE` named the managed base that goes in
    `baseImageArn`, while `default_dockerfile` hardcoded an unrelated registry
    literal in its `FROM`, so changing either left the other pointing somewhere
    else. Pairing them means a caller selects one thing and both fields follow.

    `working_dir` is empty for every public ARM64 base we measured
    (`al2023-minimal`, `python:3.12-slim`, `node:20-slim`, measured 2026-08-05), so
    an omitted `cwd` inherits nothing. It is a field rather than a lookup because a
    caller with a purpose-built image is the only one who can say what their image
    declares, and the client has no way to read it without pulling the manifest.
    """

    #: Goes into `baseImageArn` — the platform's managed base, not a registry ref.
    name: str
    #: Goes into the Dockerfile `FROM` — the registry ref measured alongside `name`.
    docker_ref: str
    #: What `docker inspect` reports for `WorkingDir`. Empty means it declares none.
    working_dir: str = ""


#: The base images this repo has actually built and run. `al2023-1` is the managed
#: base every measurement in `docs/PLATFORM.md` from 2026-08-06 onward used, paired
#: with the `amazonlinux:2023-minimal` registry ref the same builds used as `FROM`.
BASE_IMAGES: dict[str, BaseImage] = {
    "al2023-1": BaseImage(
        name="al2023-1",
        docker_ref="public.ecr.aws/amazonlinux/amazonlinux:2023-minimal",
        working_dir="",
    ),
}


def resolve_base_image(base_image: str | BaseImage) -> BaseImage:
    """Accepts a registered name or a caller's own `BaseImage`.

    A bare name must be one we have built against, because the name alone does not
    say what `FROM` pairs with it or what `WORKDIR` it declares — and guessing either
    is how the two fell out of step before. A caller on an unregistered base passes a
    `BaseImage` and states both.
    """
    if isinstance(base_image, BaseImage):
        return base_image
    try:
        return BASE_IMAGES[base_image]
    except KeyError:
        known = ", ".join(sorted(BASE_IMAGES))
        raise ValueError(
            f"unknown base image {base_image!r}. Pass one of: {known} — or a BaseImage "
            "naming its own docker_ref and working_dir, since the client cannot read "
            "either from the name."
        ) from None


def base_image_arn(region: str, name: str | BaseImage = DEFAULT_BASE_IMAGE) -> str:
    return f"arn:aws:lambda:{region}:aws:microvm-image:{resolve_base_image(name).name}"


#: Matches the first `FROM` of a Dockerfile, so a caller-supplied one can be checked
#: against the base image the create call names. Deliberately loose on whitespace and
#: case, and it ignores `--platform=`/`AS name` decoration, because the check exists
#: to catch a base that disagrees rather than to validate Dockerfile syntax.
_FROM_RE = re.compile(r"^\s*FROM\s+(?:--\S+\s+)*(\S+)", re.IGNORECASE | re.MULTILINE)


def dockerfile_from_ref(dockerfile: str) -> str | None:
    """The image ref in the Dockerfile's first `FROM`, or None if it has none."""
    match = _FROM_RE.search(dockerfile)
    return match.group(1) if match else None


def default_dockerfile(
    *,
    port: int = DEFAULT_AGENT_PORT,
    workdir: str | None = None,
    base_image: str | BaseImage = DEFAULT_BASE_IMAGE,
) -> str:
    """A Dockerfile that makes the daemon the container CMD.

    `ENTRYPOINT []` plus `CMD ["/agentd"]` is the deployment invariant the trust
    boundary rests on: it is what guarantees no task workload runs before the
    platform's run hook lands. It is also what makes an omitted `cwd` inherit the
    image WORKDIR, since the daemon's own cwd is the image's.

    The `FROM` is derived from `base_image` rather than written here, so it cannot
    disagree with the `baseImageArn` the create call sends.

    Note the invariant is *unenforced* — a base image that starts its own
    background process before bootstrap breaks it, and enforcing that belongs to
    whoever builds the image. See `docs/PROTOCOL.md`, "Trust boundary".
    """
    lines = [
        f"FROM {resolve_base_image(base_image).docker_ref}",
        "COPY agentd /agentd",
        "RUN chmod 0755 /agentd",
    ]
    if workdir:
        lines += [f"RUN mkdir -p {workdir}", f"WORKDIR {workdir}"]
    lines += [
        f"ENV AGENTD_PORT={port}",
        "ENV AGENTD_LOG=info",
        f"EXPOSE {port}",
        "ENTRYPOINT []",
        'CMD ["/agentd"]',
        "",
    ]
    return "\n".join(lines)


def build_artifact(binary: str | Path, dockerfile: str | None = None) -> bytes:
    """Zips the daemon binary with a Dockerfile, which is what the image build takes."""
    path = Path(binary)
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("Dockerfile", dockerfile or default_dockerfile())
        info = zipfile.ZipInfo("agentd")
        # The execute bit has to be set in the zip entry: a build that copies a
        # non-executable binary produces an image whose CMD fails, and the failure
        # surfaces as a run-hook timeout rather than as anything about permissions.
        info.external_attr = 0o755 << 16
        archive.writestr(info, path.read_bytes())
    return buffer.getvalue()


def default_hooks(
    port: int = DEFAULT_AGENT_PORT,
    timeout: int = DEFAULT_HOOK_TIMEOUT_SEC,
    image_timeout: int | None = None,
) -> dict[str, Any]:
    """Every hook the daemon serves, enabled.

    `ready` and `validate` are image-*build* hooks: the build calls them to decide
    whether the snapshot it just produced is usable, before any instance exists and
    therefore before any token has been delivered. Gating them on bootstrap state
    fails the build rather than the run, which is a confusing place to discover the
    mistake.

    `image_timeout` is a second parameter rather than a reuse of `timeout` because
    the two families' ceilings are 60x apart — 60s for the run-time hooks, 3600s for
    the build hooks. Defaulting it to `timeout` keeps the safe case one argument, and
    a caller who needs a long build hook can raise that family alone instead of
    raising both and being rejected on the run family only.
    """
    hooks = {
        "port": port,
        "microvmImageHooks": {
            "ready": "ENABLED",
            "readyTimeoutInSeconds": timeout if image_timeout is None else image_timeout,
            "validate": "ENABLED",
            "validateTimeoutInSeconds": timeout if image_timeout is None else image_timeout,
        },
        "microvmHooks": {
            "run": "ENABLED",
            "runTimeoutInSeconds": timeout,
            "suspend": "ENABLED",
            "suspendTimeoutInSeconds": timeout,
            "resume": "ENABLED",
            "resumeTimeoutInSeconds": timeout,
            "terminate": "ENABLED",
            "terminateTimeoutInSeconds": timeout,
        },
    }
    return require_hook_timeouts_in_range(hooks)


#: Measured 2026-08-05. Not `/aws/lambda/microvms/*` — an IAM policy granting that
#: prefix instead produces server-side builds with no logs at all, and then every
#: build failure reads `reason=unknown`, which looks like the service failing to
#: populate `stateReason` when it is the caller's own policy discarding the evidence.
BUILD_LOG_GROUP_PREFIX = "/aws/lambda-microvms"


@dataclass
class Image:
    """A built MicroVM image, and the log group the service created alongside it."""

    identifier: str
    name: str
    #: The class the requested baseline selected. Carried on the image because
    #: billing follows the baseline that was requested at *create* time, and by the
    #: time anyone asks what a run cost the request is gone.
    size: SizeClass = field(default_factory=lambda: size_class_for(DEFAULT_BASELINE_MIB))

    @property
    def build_log_group(self) -> str:
        """`/aws/lambda-microvms/<image-name>`, not `/aws/lambda/microvms/*`.

        The service creates this itself, so a Terraform stack never owns it and
        `terraform destroy` leaves it behind — "the stack destroyed cleanly" is not
        "the account is clean". Delete it in teardown.
        """
        return f"{BUILD_LOG_GROUP_PREFIX}/{self.name}"


class Sandbox:
    """One MicroVM's lifecycle: build, run, suspend, resume, terminate.

    `session` is available once the VM is RUNNING. Suspend and resume preserve it:
    measured 2026-08-05, a suspend/resume cycle keeps the in-memory agent token,
    the filesystem, exec records including unacked output, running processes, and
    the endpoint URL. So a warm suspended pool is viable and needs no
    re-bootstrapping.
    """

    def __init__(
        self,
        *,
        region: str = "us-east-1",
        port: int = DEFAULT_AGENT_PORT,
        microvm_client: Any = None,
        logs_client: Any = None,
        s3_client: Any = None,
        poll_interval_sec: float = DEFAULT_POLL_INTERVAL_SEC,
        clock: Callable[[], float] = time.monotonic,
        allow_unlisted_region: bool = False,
    ) -> None:
        # Before the client is built, because building one succeeds for any region and
        # the first call is where the evidence is lost to a null-message denial.
        require_supported_region(region, allow_unlisted=allow_unlisted_region)
        self.region = region
        self.port = port
        self.poll_interval_sec = poll_interval_sec
        # Monotonic, and injectable. Monotonic because the suspended window is a
        # duration and a wall clock that steps backward would reopen a closed one.
        # Injectable because the only other way to test a 600-second window is to
        # wait 600 seconds.
        self._clock = clock
        self._mv = microvm_client or _client(SERVICE, region)
        self._logs = logs_client
        self._s3 = s3_client
        self.image: Image | None = None
        self.microvm_id: str | None = None
        self.agent_token: str | None = None
        self.session: Session | None = None
        # The launch-time idlePolicy window, recorded because `resume` needs it and
        # `GetMicrovm` does not return it: the value only exists in the RunMicrovm
        # request, so a resume path that wants to name the window has to remember it.
        self._suspended_window_sec: int | None = None
        self._suspended_at: float | None = None

    def __enter__(self) -> Sandbox:
        return self

    def __exit__(self, *_: object) -> None:
        self.terminate()

    # -- image -------------------------------------------------------------

    def build_image(
        self,
        *,
        name: str,
        binary: str | Path,
        bucket: str,
        build_role_arn: str,
        dockerfile: str | None = None,
        base_image: str | BaseImage = DEFAULT_BASE_IMAGE,
        architecture: str = "ARM_64",
        memory_mib: int = DEFAULT_BASELINE_MIB,
        hooks: dict[str, Any] | None = None,
        tags: dict[str, str] | None = None,
        repair_guest_identity: bool = False,
        inherit_workdir: bool = False,
        build_timeout_sec: float = DEFAULT_IMAGE_BUILD_TIMEOUT_SEC,
        token_scope: str | None = None,
    ) -> Image:
        """Uploads the artifact, creates the image, and waits for it to be usable.

        `memory_mib` selects a documented size class, it does not size the VM: the
        guest will report roughly four times what is asked for, and the billed rate
        follows the request. `sizing.size_class_for` names both numbers.

        `token_scope` is a label folded into the create token for readability in
        CloudTrail. It is not the token — see `_create_token` for why the client will
        not accept one.
        """
        # Every local rejection happens before `put_object`, which is the whole point:
        # the create call the service would reject comes *after* the artifact upload,
        # so letting AWS answer costs the upload and the wait before the answer.
        require_valid_image_name(name)
        if architecture not in ARCHITECTURES:
            raise ValueError(
                f"architecture={architecture!r} is not in the Architecture enum "
                f"{list(ARCHITECTURES)} (service model {MODEL_API_VERSION}). MicroVMs are "
                "ARM64-only, so a host-built x86 binary is the most common first-attempt "
                "failure — and it surfaces as a run-hook timeout, which says nothing "
                "about architecture."
            )
        base = resolve_base_image(base_image)
        size = size_class_for(memory_mib)
        if hooks is not None:
            require_hook_timeouts_in_range(hooks)
        if inherit_workdir:
            self._require_workdir(base, dockerfile)
        if dockerfile is not None:
            self._require_matching_from(base, dockerfile)

        s3 = self._s3 or _client("s3", self.region)
        key = f"{name}.zip"
        s3.put_object(
            Bucket=bucket,
            Key=key,
            Body=build_artifact(binary, dockerfile or default_dockerfile(port=self.port)),
        )

        created = self._mv.create_microvm_image(
            name=name,
            baseImageArn=base_image_arn(self.region, base),
            buildRoleArn=build_role_arn,
            codeArtifact={"uri": f"s3://{bucket}/{key}"},
            cpuConfigurations=[{"architecture": architecture}],
            resources=[{"minimumMemoryInMiB": size.baseline_mib}],
            hooks=hooks or default_hooks(self.port),
            tags=tags or {},
            # Measured 2026-08-06, us-east-1: without this, a guest running as root
            # still gets EPERM from `sethostname` and from a bind mount over
            # `/proc/sys/kernel/random/boot_id`, because the MicroVM drops
            # CAP_SYS_ADMIN by default. Writing `/etc/machine-id` needs no
            # capability and succeeds either way, which is what makes the gap easy
            # to miss: identity repair looks like it works until you check the two
            # steps that need the kernel's permission rather than the filesystem's.
            #
            # "ALL" is the only accepted value in the 2025-09-09 API — there is no
            # way to request CAP_SYS_ADMIN alone — which is why the parameter above
            # is a boolean naming the *intent*. A list would let a caller write
            # ["CAP_SYS_ADMIN"], the request AWS rejects after the artifact upload.
            **({"additionalOsCapabilities": ["ALL"]} if repair_guest_identity else {}),
            clientToken=_create_token(token_scope or name),
        )
        identifier = created.get("imageIdentifier") or created.get("imageArn")
        image = Image(identifier=str(identifier), name=name, size=size)
        self.image = image
        self._wait_for_image(image, deadline=time.time() + build_timeout_sec)
        return image

    @staticmethod
    def _require_workdir(base: BaseImage, dockerfile: str | None) -> None:
        """Rejects working-directory inheritance when nothing declares one.

        Measured 2026-08-05: `al2023-minimal`, `python:3.12-slim`, and `node:20-slim`
        all leave `WorkingDir` empty, so "inherit the image WORKDIR" inherits `/` and
        every relative path in the caller's commands resolves somewhere they did not
        mean. Rejected rather than warned because the symptom appears in the *guest*,
        one build cycle later, as commands that run in the wrong directory rather than
        as anything about WORKDIR.
        """
        if base.working_dir:
            return
        if dockerfile is not None and re.search(r"^\s*WORKDIR\s+\S", dockerfile, re.MULTILINE):
            return
        raise ValueError(
            f"inherit_workdir=True but base image {base.name!r} declares no WorkingDir and "
            "the Dockerfile sets none. Most public ARM64 base images leave it empty "
            "(docs/PLATFORM.md, 'Most public ARM64 base images have no WORKDIR'), so there "
            "is nothing to inherit — pass `default_dockerfile(workdir=...)` or set WORKDIR "
            "in your own Dockerfile."
        )

    @staticmethod
    def _require_matching_from(base: BaseImage, dockerfile: str) -> None:
        """Rejects a Dockerfile whose `FROM` is not the selected base image.

        The build runs the Dockerfile *on top of* the base named in `baseImageArn`, so
        the two disagreeing produces an image built from something other than the
        platform base whose behavior every measurement in `docs/PLATFORM.md` describes
        — and nothing in the result says so.
        """
        found = dockerfile_from_ref(dockerfile)
        if found is None or found == base.docker_ref:
            return
        raise ValueError(
            f"the Dockerfile's FROM is {found!r} but base_image {base.name!r} pairs with "
            f"{base.docker_ref!r}. These must agree: `baseImageArn` and the FROM select the "
            "same base, and a mismatch builds against a base none of the measured platform "
            "behavior applies to. Use `default_dockerfile(base_image=...)`, or pass a "
            "BaseImage whose docker_ref matches."
        )

    def _wait_for_image(self, image: Image, *, deadline: float) -> dict[str, Any]:
        """Waits for CREATED, distinguishing a stalled build from a slow one.

        CREATING covers both a build in progress and a build the service never
        scheduled — the clientToken-replay signature above. Probing the build list
        after a grace period turns a 45-minute silent wait into an actionable
        failure.
        """
        image_id = image.identifier
        started = time.time()
        probed = False
        while time.time() < deadline:
            got = self._mv.get_microvm_image(imageIdentifier=image_id)
            state = got.get("state")
            if state in IMAGE_READY_STATES:
                return got
            if state and "FAILED" in state:
                raise RuntimeError(
                    self._build_failure_message(image, state, got.get("stateReason"))
                )

            elapsed = time.time() - started
            if not probed and elapsed > STALL_GRACE_SEC:
                probed = True
                self._probe_stalled_build(image_id, elapsed)
            time.sleep(max(self.poll_interval_sec, 15.0))
        raise RuntimeError(f"image {image_id} did not become usable in time")

    def _build_failure_message(self, image: Image, state: str, reason: Any) -> str:
        """Explains a build failure, and names the log-group prefix when there are no logs.

        Measured 2026-08-05: a build role granted `/aws/lambda/microvms/*` — the
        plausible spelling, and the wrong one — produces server-side builds that write
        no logs at all. Every failure then reads `reason=unknown`, which looks like the
        service failing to populate `stateReason` and sends the reader to investigate
        AWS rather than their own policy. Forwarding `stateReason` verbatim is what
        makes that indistinguishable, so an empty log group gets named as the more
        likely cause than a silent service.
        """
        base = f"image build failed: {state} {reason}"
        if self._build_log_group_is_empty(image):
            return (
                f"{base} — and the build log group {image.build_log_group!r} contains no "
                f"events, so the reason above is all the evidence there is. The build role "
                f"must grant logs on the {BUILD_LOG_GROUP_PREFIX}/* prefix; a policy "
                f"granting /aws/lambda/microvms/* instead produces builds with no logs and "
                f"failures that read as unknown (docs/PLATFORM.md, 'Build logs go to "
                f"{BUILD_LOG_GROUP_PREFIX}/<image-name>')."
            )
        return base

    def _build_log_group_is_empty(self, image: Image) -> bool:
        """True only when CloudWatch answers and answers empty.

        A logs client that throws, or that is absent because the caller passed none,
        must read as "unknown" rather than "empty": claiming a misconfigured IAM prefix
        on the strength of a throttled API call would send the reader after the wrong
        cause, which is the failure this whole path exists to prevent.
        """
        logs = self._logs
        if logs is None:
            with contextlib.suppress(Exception):
                logs = _client("logs", self.region)
        if logs is None:
            return False
        try:
            streams = logs.describe_log_streams(logGroupName=image.build_log_group)
        except Exception:  # noqa: BLE001 - unknown is not empty; see the docstring
            return False
        return not any(s.get("lastEventTimestamp") for s in streams.get("logStreams", []))

    def _probe_stalled_build(self, image_id: str, elapsed: float) -> None:
        try:
            versions = self._mv.list_microvm_image_versions(imageIdentifier=image_id)
            version = versions["items"][0]["imageVersion"]
            builds = self._mv.list_microvm_image_builds(
                imageIdentifier=image_id, imageVersion=version
            )
            # `buildState`, not `state`. The model's `MicrovmImageBuildSummary` has no
            # `state` member at all, so the previous `b.get("state")` read `None` from
            # every real response and the all-PENDING test below could never be true.
            # This guard — the only thing that separates a wedged image from a slow
            # build, and the one that would have caught the ~15-hour wedge — was dead
            # against live AWS while passing its unit test, because the fake returned
            # `{"state": "PENDING"}`: the fake shared the client's own misreading.
            # Found 2026-08-07 by diffing against the service model.
            states = [b.get("buildState") for b in builds.get("items", [])]
        except Exception:  # noqa: BLE001 - a best-effort probe must never break the wait
            return
        if states and all(s == "PENDING" for s in states):
            raise RuntimeError(
                f"build never scheduled after {elapsed:.0f}s: all builds still PENDING "
                f"({states}). This is the clientToken replay signature — the image is "
                "wedged and cannot be deleted."
            )

    # -- run ---------------------------------------------------------------

    def run(
        self,
        *,
        image_identifier: str | None = None,
        execution_role_arn: str,
        agent_token: str | None = None,
        max_idle_sec: int = 600,
        suspended_sec: int = 600,
        auto_resume: bool = False,
        max_duration_sec: int = 3600,
        egress: bool = False,
        ready_timeout_sec: float = 300.0,
        token_scope: str | None = None,
    ) -> Session:
        """Launches a MicroVM, waits for RUNNING, and returns its `Session`.

        The agent token is delivered through `runHookPayload`, which is what keeps
        it out of the shared image snapshot. That is safe because the platform
        forwards no external traffic until the run hook returns 200 — so a per-VM
        secret delivered at launch wins the first-writer race through the endpoint.

        The daemon reads it one JSON parse deeper than you would expect: the
        platform wraps the string, so the hook body is
        `{"runHookPayload": "{\\"agent_token\\": \\"...\\"}"}`.

        `suspended_sec` is a sharp edge for anyone planning to suspend
        deliberately: the launch-time idle policy *terminates* a suspended VM after
        that window, so a "resume later" affordance silently stops working once it
        passes. `resume` reads the value recorded here to say so.
        """
        identifier = image_identifier or (self.image.identifier if self.image else None)
        if not identifier:
            raise ValueError("no image: pass image_identifier or call build_image first")
        require_duration_in_range(max_duration_sec)

        token = agent_token or secrets.token_urlsafe(32)
        # Checked even though the client builds the payload itself, because
        # `agent_token` is a caller-supplied value: a caller who passes a JWT or a
        # signed blob rather than a bearer token is who this catches.
        payload = require_payload_fits(json.dumps({"agent_token": token}))
        kwargs: dict[str, Any] = {
            "imageIdentifier": identifier,
            "executionRoleArn": execution_role_arn,
            "ingressNetworkConnectors": [connector_arn(self.region, NetworkConnector.ALL_INGRESS)],
            "idlePolicy": {
                # Bounds the cost of a crashed caller: an abandoned VM suspends and
                # then terminates instead of billing to the maximumDuration ceiling.
                # Idle is measured by inbound traffic through the proxy.
                "maxIdleDurationSeconds": max_idle_sec,
                "suspendedDurationSeconds": suspended_sec,
                "autoResumeEnabled": auto_resume,
            },
            "maximumDurationInSeconds": max_duration_sec,
            "runHookPayload": payload,
            "clientToken": _run_token(token_scope or identifier),
        }
        if egress:
            kwargs["egressNetworkConnectors"] = [
                connector_arn(self.region, NetworkConnector.INTERNET_EGRESS)
            ]

        run = self._mv.run_microvm(**kwargs)
        self.microvm_id = run["microvmId"]
        self.agent_token = token
        self._suspended_window_sec = suspended_sec
        endpoint = self._wait_for_running(run.get("endpoint", ""), timeout=ready_timeout_sec)

        self.session = Session(
            endpoint=endpoint,
            agent_token=token,
            microvm_id=self.microvm_id,
            microvm_client=self._mv,
            port=self.port,
        )
        return self.session

    def _wait_for_running(self, endpoint: str, *, timeout: float) -> str:
        """Polls to RUNNING, failing fast on a terminal state with `stateReason`.

        Failing fast is the whole value: a VM that reaches a terminal state before
        RUNNING died during startup, and for a hook-serving daemon that almost
        always means a lifecycle hook failed. Polling through it wastes minutes and
        then reports a connection error that hides the cause — and by then the VM is
        gone, so `stateReason` is the only evidence left.
        """
        assert self.microvm_id is not None
        deadline = time.time() + timeout
        while time.time() < deadline:
            got = self._mv.get_microvm(microvmIdentifier=self.microvm_id)
            state = got.get("state")
            if state == "RUNNING":
                return got.get("endpoint") or endpoint
            if state in TERMINAL_STATES:
                raise RuntimeError(
                    f"microvm {self.microvm_id} reached {state} before RUNNING: "
                    f"{got.get('stateReason') or 'no stateReason'}"
                )
            time.sleep(self.poll_interval_sec)
        raise RuntimeError(f"microvm {self.microvm_id} never reached RUNNING within {timeout:.0f}s")

    # -- suspend / resume / terminate --------------------------------------

    def suspend(self, *, timeout: float = 300.0) -> str:
        """Freezes the VM. Returns the state reached.

        A freeze and restore, not a stop and start: the guest keeps its memory, so
        everything survives. The one thing that does not is the guest's view of
        time — it observes the whole suspension as a single jump, so any timeout,
        lease, or TLS session a running command holds expires at once on resume.
        """
        if not self.microvm_id:
            raise RuntimeError("nothing to suspend")
        self._mv.suspend_microvm(microvmIdentifier=self.microvm_id)
        # Stamped before the wait, not after: the idlePolicy's window starts when the
        # platform begins suspending, so timing it from SUSPENDED would under-count the
        # transition and call a closed window open.
        self._suspended_at = self._clock()
        return self._wait_for_state({"SUSPENDED", "TERMINATED"}, timeout=timeout)

    def resume(self, *, timeout: float = 300.0) -> Session:
        """Thaws the VM and returns a usable `Session`.

        No token re-delivery and no re-bootstrap: the in-memory token survived. The
        session's proxy token is dropped, because a token minted against the
        pre-suspend instance is not guaranteed to validate and that rejection reads
        exactly like a dead daemon.

        Two ways this refuses rather than hangs, both because the launch-time
        `idlePolicy` *terminates* a suspended VM after `suspendedDurationSeconds` —
        so "resume later" silently stops working once that window passes, and the
        VM is gone rather than slow.
        """
        if not self.microvm_id or self.session is None:
            raise RuntimeError("nothing to resume")
        self._require_open_suspended_window()
        self._mv.resume_microvm(microvmIdentifier=self.microvm_id)
        state = self._wait_for_state({"RUNNING"}, timeout=timeout, fail_on=DEAD_STATES)
        if state != "RUNNING":
            raise RuntimeError(f"resume left the microvm in {state}")
        got = self._mv.get_microvm(microvmIdentifier=self.microvm_id)
        self.session.rebind(got.get("endpoint") or self.session.endpoint)
        # Cleared on success so the next cycle's window is measured from the next
        # suspend. Leaving it set would accumulate every suspension's elapsed time
        # into one total and reject a resume whose own window is wide open.
        self._suspended_at = None
        return self.session

    def _require_open_suspended_window(self) -> None:
        """Rejects a resume the launch-time `idlePolicy` has already made impossible.

        Checked locally, before `ResumeMicrovm`, because the answer is already known:
        the window came from *our own* `RunMicrovm` request and `GetMicrovm` does not
        return it, so the client is the only party that can name the number. Calling
        first and reading the failure would report whatever the service says about a
        terminated id, which is not the same as "the window you set at launch closed".
        """
        window = self._suspended_window_sec
        if window is None or self._suspended_at is None:
            return
        elapsed = self._clock() - self._suspended_at
        if elapsed <= window:
            return
        raise RuntimeError(
            f"microvm {self.microvm_id} has been suspended {elapsed:.0f}s, past the "
            f"{window}s suspendedDurationSeconds window set at launch — the idlePolicy "
            f"terminates a suspended VM once that window passes, so there is nothing left "
            f"to resume (docs/PLATFORM.md, '`idlePolicy`')."
        )

    def _wait_for_state(
        self, want: set[str], *, timeout: float, fail_on: frozenset[str] = frozenset()
    ) -> str:
        """Polls until a wanted state, failing fast on any `fail_on` state.

        `fail_on` exists because the resume path had the exact defect
        `_wait_for_running` was written to prevent. A VM the `idlePolicy` terminated
        during suspension never reaches RUNNING, so waiting only for RUNNING burned
        the full 300s and then reported "never reached RUNNING" — a timeout message
        that hides a cause the service had already stated in `stateReason`.

        Empty by default rather than always `DEAD_STATES`, because `suspend` *wants*
        TERMINATED: a VM that dies while suspending is a state to report, not an
        exception to raise out of the middle of a teardown.
        """
        assert self.microvm_id is not None
        deadline = time.time() + timeout
        while time.time() < deadline:
            got = self._mv.get_microvm(microvmIdentifier=self.microvm_id)
            state = got.get("state")
            if state in want:
                return str(state)
            if state in fail_on:
                raise RuntimeError(
                    f"microvm {self.microvm_id} is {state}, so it will never reach "
                    f"{sorted(want)}: {got.get('stateReason') or 'no stateReason'}. A "
                    f"suspended VM is terminated once the launch-time "
                    f"suspendedDurationSeconds window ({self._suspended_window_sec}s) "
                    f"passes (docs/PLATFORM.md, '`idlePolicy`')."
                )
            time.sleep(self.poll_interval_sec)
        raise RuntimeError(f"microvm {self.microvm_id} never reached {want} within {timeout:.0f}s")

    def terminate(self, *, delete_image: bool = False, delete_log_group: bool = False) -> None:
        """Tears down, best-effort, never raising.

        Never raising because this runs in a `finally`: an exception here replaces
        the real failure with a teardown failure, and the real failure is the one
        worth reading.
        """
        if self.session is not None:
            self.session.close()
            self.session = None
        if self.microvm_id:
            with contextlib.suppress(Exception):
                self._mv.terminate_microvm(microvmIdentifier=self.microvm_id)
        if delete_log_group and self.image:
            self.delete_build_log_group()
        if delete_image and self.image:
            self.delete_image()

    def delete_build_log_group(self) -> bool:
        """Deletes the log group the service created for the build.

        Separate from `terraform destroy`, which never owned it. Six of these
        accumulated before anyone noticed — storage-only cost, but a leak is a leak.
        """
        if not self.image:
            return False
        logs = self._logs or _client("logs", self.region)
        try:
            logs.delete_log_group(logGroupName=self.image.build_log_group)
        except Exception:  # noqa: BLE001
            return False
        return True

    def delete_image(self, *, attempts: int = 20, backoff_sec: float = 15.0) -> bool:
        """Deletes the image and every version but the last, retrying.

        Retrying is not politeness: an image in CREATING refuses deletion, and a VM
        still terminating holds a reference. This retry loop is the difference
        between a clean account and a billed leak.
        """
        if not self.image:
            return False
        image_id = self.image.identifier
        for _ in range(attempts):
            try:
                versions = self._mv.list_microvm_image_versions(imageIdentifier=image_id)
                # Every version but the first: the last remaining version cannot be
                # deleted on its own, only with the image.
                for item in versions.get("items", [])[1:]:
                    self._mv.delete_microvm_image_version(
                        imageIdentifier=image_id, imageVersion=item["imageVersion"]
                    )
                self._mv.delete_microvm_image(imageIdentifier=image_id)
                return True
            except Exception:  # noqa: BLE001
                time.sleep(backoff_sec)
        return False


def _client(service: str, region: str) -> Any:
    """A boto3 client with standard retries, imported lazily.

    Lazy so `Session` and the error taxonomy stay importable without boto3 or
    credentials, which is what makes the test suite runnable with no AWS at all.
    """
    import boto3
    from botocore.config import Config as BotoConfig

    return boto3.Session(region_name=region).client(
        service, config=BotoConfig(retries={"max_attempts": 10, "mode": "standard"})
    )
