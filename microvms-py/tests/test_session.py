# SPDX-License-Identifier: Apache-2.0
"""The session surface: `src/session.rs`, plus the hooks in `src/hooks.rs`.

# What a unit run can say about a session

Constructing one does not talk to the VM — deliberately, because a constructor that probed would
make "do I have a session" mean "is the VM up", and those are different questions with different
answers during a launch. So `Session.direct(...)` is fully testable offline, and so is the
*argument* half of every method on it.

What a unit run cannot say is whether a `run` starts a process, because that needs the daemon.
The tests below therefore assert three things and stop:

* the **construction** contract — what is reachable, what is absent, and what a direct session
  reports about itself;
* the **argument** contract — which command shapes napi/PyO3 accept before any Rust runs, and
  which are refused;
* the **failure taxonomy** of a request that cannot connect, because a caller's retry logic
  branches on it and getting it wrong turns a dead VM into an infinite loop.

The last one is the reason a `127.0.0.1:9` endpoint appears below rather than a mock: a refused
connection is a real transport failure with a real answer, and it is reachable without inventing
a fake daemon whose agreement with the real one nobody would be testing.
"""

from __future__ import annotations

import pytest

import microvms


def direct() -> microvms.Session:
    """A session against an endpoint nothing is listening on."""
    return microvms.Session.direct("http://127.0.0.1:9", "agent-token")


# -- construction --------------------------------------------------------------


def test_a_direct_session_reports_its_endpoint_and_the_default_port() -> None:
    """The conformance shape: no proxy headers, no control plane, no credentials.

    `direct` is a supported path rather than a test hatch — it is what a caller inside the VM or
    on a tunnel uses — so it has to work with nothing configured.
    """
    session = microvms.Session.direct("http://127.0.0.1:9000", "agent-token")
    assert session.endpoint == "http://127.0.0.1:9000"
    assert session.port == microvms.session_constants()["defaultAgentPort"]


def test_a_direct_session_has_no_minter_so_nothing_mints() -> None:
    """`None` and not zero, and the difference is the whole point.

    Zero would say "this session mints and has not yet"; `None` says "this session does not
    mint". A direct session sends no proxy headers at all, and a monitor watching for a stale
    token (STATE-8) must not read a direct session as one that never refreshed.
    """
    assert direct().proxy_mint_count is None


def test_there_is_no_constructor_so_a_session_comes_from_direct_or_from_a_sandbox() -> None:
    """Two doors, both named.

    A `Session(...)` constructor would be a third, and the thing it would most plausibly take is
    a proxy token — which is exactly what TRAP-9 says a caller must not hand in, because minting
    happens inside every request and a token passed in is one that expires mid-run.
    """
    with pytest.raises(TypeError):
        microvms.Session()  # type: ignore[call-arg]
    with pytest.raises(TypeError):
        microvms.Session("http://127.0.0.1:9000", "token")  # type: ignore[call-arg]


def test_no_proxy_token_is_reachable_anywhere_on_the_session_surface() -> None:
    """TRAP-7 by absence: there is nothing to treat as a string.

    The core's `ProxyToken` has no `Display`, no `as_str`, and no `Deref`, and the binding adds
    no accessor — so "log the auth token" is as inexpressible here as it is in Rust. Asserted as
    the absence of every name someone would reach for.
    """
    session = direct()
    for attribute in (
        "proxy_token",
        "token",
        "auth_token",
        "agent_token",
        "proxy_auth",
        "headers",
    ):
        assert not hasattr(session, attribute), f"{attribute} is reachable"
    # The one observable that *is* exposed is a count, which carries no secret.
    assert session.proxy_mint_count is None


def test_the_repr_names_the_endpoint_and_port_without_the_token() -> None:
    """A debug form that is safe to paste into a bug report.

    An endpoint and a port are addresses; the agent token is a credential. A repr carrying it
    would put a bearer token into every traceback and log line that formatted a session.
    """
    rendered = repr(microvms.Session.direct("http://127.0.0.1:9000", "super-secret-token"))
    assert "127.0.0.1:9000" in rendered
    assert "9000" in rendered
    assert "super-secret-token" not in rendered


# -- the command contract ------------------------------------------------------


@pytest.mark.parametrize("bad", [3, 3.5, None, True, {"cmd": "ls"}, object()])
def test_a_non_command_is_refused_by_the_extraction_before_a_request_is_built(
    bad: object,
) -> None:
    """A `TypeError` from PyO3's own conversion, not a check written in the binding.

    Stronger than a refusal inside the method: the extraction runs before any Rust body does, so
    no request was built and nothing reached the daemon. A caller who passed a dict got the error
    at the call site rather than as a 400 from the VM.
    """
    with pytest.raises(TypeError):
        direct().run(bad)  # type: ignore[arg-type]
    with pytest.raises(TypeError):
        direct().run_sync(bad)  # type: ignore[arg-type]


def test_both_command_spellings_are_accepted_and_neither_is_whitespace_split() -> None:
    """`run("ls -la")` is a **one-element** argv, which is `session.py`'s own rule.

    Splitting on spaces is how `/opt/my app/bin/tool` becomes two arguments nobody meant. There
    is no daemon here to read the built request back from, so what is asserted is the reachable
    half: both spellings get past the extraction and fail on the *wire* rather than on the type —
    which is what says the argument itself was accepted.
    """
    for command in ("ls -la", ["ls", "-la"]):
        with pytest.raises(microvms.MicrovmError) as raised:
            direct().run(command)
        # Past the extraction: the failure is a transport one, not a `TypeError`.
        assert raised.value.wire_kind == "Transport"


def test_an_empty_argv_reaches_the_daemon_rather_than_being_refused_locally() -> None:
    """BIND-2: a check here would be the copy nothing else tests.

    An empty argv is a real mistake, and the daemon is what refuses it — the core has no local
    guard, so neither does the binding. Documented as a test because "why is this not validated"
    is the obvious question, and the answer is that the refusal belongs in one place.
    """
    with pytest.raises(microvms.MicrovmError) as raised:
        direct().run([])
    assert raised.value.wire_kind == "Transport", "an empty argv was refused locally"


def test_every_exec_option_is_keyword_only_so_none_can_be_transposed() -> None:
    """One positional parameter — the command — and the rest by name.

    `run(cmd, False, None, None, 1000, 1000)` is a signature where `user` and `group` transpose
    silently, and both are plausible integers. Keyword-only makes that unwriteable.
    """
    session = direct()
    with pytest.raises(TypeError):
        session.run(["ls"], True)  # type: ignore[misc]
    # And the names really are the ones documented.
    with pytest.raises(microvms.MicrovmError):
        session.run(
            ["ls"],
            shell=False,
            cwd="/tmp",
            env={"KEY": "value"},
            user=1000,
            group=1000,
            timeout_sec=30.0,
            stdin=True,
            exec_id="x-0000000000000009",
        )


def test_a_supplied_exec_id_is_the_idempotency_key_and_comes_back_on_the_handle() -> None:
    """What a caller whose retry must be safe across its own restart passes.

    Asserted through the reattach path, which needs no daemon: the handle carries the id it was
    given, so a second process with the same id addresses the same server-side exec.
    """
    session = direct()
    assert session.exec("x-00000000000000ff").exec_id == "x-00000000000000ff"


def test_the_octal_mode_is_a_string_because_an_integer_would_be_ambiguous() -> None:
    """`"0755"` and not `0o755` or `755`.

    An integer parameter cannot distinguish the two readings, and they differ: `755` decimal is
    not a mode anyone means. A string is the daemon's own shape, so nothing here converts.
    """
    session = direct()
    with pytest.raises(TypeError):
        session.upload_file("/tmp/x", b"data", mode=0o755)  # type: ignore[arg-type]
    # A string gets through to the wire.
    with pytest.raises(microvms.MicrovmError) as raised:
        session.upload_file("/tmp/x", b"data", mode="0755")
    assert raised.value.wire_kind == "Transport"


def test_file_transfer_takes_bytes_rather_than_str_so_no_encode_is_implied() -> None:
    """An upload is bytes; a `str` would need an encoding this layer must not pick.

    Same reasoning as `OutputChunk.data`: the file's contents are whatever they are, and a
    silent UTF-8 encode would corrupt anything that was not text.
    """
    session = direct()
    with pytest.raises(TypeError):
        session.upload_file("/tmp/x", "text")  # type: ignore[arg-type]
    with pytest.raises(TypeError):
        session.upload_tar("/tmp", "not bytes")  # type: ignore[arg-type]


# -- the failure taxonomy of an unreachable daemon -----------------------------


def test_a_refused_connection_is_retryable_on_every_method_that_makes_a_request() -> None:
    """The branch a caller's retry loop reads, checked across the surface rather than once.

    A refused connection says nothing about the VM or the exec — it is exactly what a VM that
    has just reached RUNNING does for a moment before the proxy path is wired up — so every one
    of these has to be retryable. One method reporting it as fatal would make a caller give up
    on a VM that was about to come good; one reporting a genuine 401 as retryable would loop
    until the deadline. The pair is the reason the taxonomy exists.
    """
    session = direct()
    for name, call in (
        ("health", session.health),
        ("run", lambda: session.run(["true"])),
        ("run_sync", lambda: session.run_sync(["true"])),
        ("kill", lambda: session.kill("x-0000000000000001")),
        ("file_exists", lambda: session.file_exists("/tmp/x")),
        ("download_file", lambda: session.download_file("/tmp/x")),
        ("download_tar", lambda: session.download_tar("/tmp")),
        ("upload_file", lambda: session.upload_file("/tmp/x", b"data")),
        ("upload_tar", lambda: session.upload_tar("/tmp", b"data")),
    ):
        with pytest.raises(microvms.MicrovmError) as raised:
            call()
        error = raised.value
        assert error.retryable is True, f"{name} reported a refused connection as fatal"
        assert error.code == "ERR_RETRYABLE", name
        assert error.wire_kind == "Transport", name


def test_a_transport_failure_names_the_method_and_path_it_was_attempting() -> None:
    """The message says *which* request failed, which is what makes a log line actionable.

    "error sending request" alone leaves a reader unable to tell a health probe from a file
    download — and during a launch those mean quite different things.
    """
    with pytest.raises(microvms.MicrovmError) as raised:
        direct().health()
    message = str(raised.value)
    assert "GET" in message
    assert "/v1/health" in message


# -- the hook timeouts (`src/hooks.rs`) ----------------------------------------


def test_the_two_hook_families_have_the_ceilings_the_service_documents() -> None:
    """60 and 3600, sixty times apart — which is why they are two types."""
    assert microvms.RunHookTimeout.MAX_SECS == 60
    assert microvms.BuildHookTimeout.MAX_SECS == 3600


@pytest.mark.parametrize("seconds", [1, 30, 60])
def test_the_run_family_accepts_its_whole_documented_range(seconds: int) -> None:
    """Including the boundary, because an off-by-one at 60 would refuse a legal value."""
    assert microvms.RunHookTimeout(seconds).seconds == seconds


@pytest.mark.parametrize("seconds", [1, 60, 3600])
def test_the_build_family_accepts_its_whole_documented_range(seconds: int) -> None:
    """3600 is legal here and refused for the run family — the asymmetry is the point."""
    assert microvms.BuildHookTimeout(seconds).seconds == seconds


@pytest.mark.parametrize("seconds", [0, 61, 3600, 100_000])
def test_the_run_family_refuses_everything_above_its_ceiling_and_zero(seconds: int) -> None:
    """Zero as well as the overshoots: a zero-second hook cannot complete.

    3600 is in this list on purpose — it is the *build* family's ceiling, so it is the number
    someone reaches for after reading the other type's documentation.
    """
    with pytest.raises(microvms.InvalidArgError) as raised:
        microvms.RunHookTimeout(seconds)
    assert raised.value.code == "ERR_INVALID_ARG"


@pytest.mark.parametrize("seconds", [0, 3601, 100_000])
def test_the_build_family_refuses_everything_above_its_own_ceiling_and_zero(
    seconds: int,
) -> None:
    """The mirror, so neither type is the lax one."""
    with pytest.raises(microvms.InvalidArgError):
        microvms.BuildHookTimeout(seconds)


def test_the_refusal_names_both_ceilings_because_the_caller_picked_the_other_one() -> None:
    """Telling someone "the limit is 60" answers a question they did not ask.

    A caller who passes 3600 to the run family is nearly always someone who read the build
    family's limit, so the message names both — which turns the refusal into an instruction.
    """
    with pytest.raises(microvms.InvalidArgError) as raised:
        microvms.RunHookTimeout(3600)
    message = str(raised.value)
    assert "60" in message
    assert "3600" in message


def test_the_two_timeout_types_are_not_interchangeable_in_either_direction() -> None:
    """BIND-2's clearest case, asserted as the absence of a conversion.

    Neither type extracts from the other, so a transposed pair is a `TypeError` from PyO3's
    argument conversion rather than a silently accepted 60 where 3600 was meant. There is no
    method here that takes one without a daemon, so the check is on the types themselves: no
    shared base, and no constructor accepting the sibling.
    """
    run = microvms.RunHookTimeout(30)
    build = microvms.BuildHookTimeout(30)
    assert type(run) is not type(build)
    assert not isinstance(run, type(build))
    assert not isinstance(build, type(run))
    # Neither constructor takes the other type, so the transposition cannot be written.
    with pytest.raises(TypeError):
        microvms.RunHookTimeout(build)  # type: ignore[arg-type]
    with pytest.raises(TypeError):
        microvms.BuildHookTimeout(run)  # type: ignore[arg-type]


# -- the session constants -----------------------------------------------------


def test_both_proxy_headers_are_published_because_one_without_the_other_is_rejected() -> None:
    """TRAP-7: they go out together or the request is refused indistinguishably.

    Published so a harness asserting against the wire contract does not hardcode a spelling, and
    checked as a pair because sending one is the failure mode.
    """
    constants = microvms.session_constants()
    assert "proxy-auth" in constants["proxyAuthHeader"].lower()
    assert "proxy-port" in constants["proxyPortHeader"].lower()
    assert constants["proxyAuthHeader"] != constants["proxyPortHeader"]


def test_the_refresh_window_is_inside_the_token_lifetime_with_room_to_spare() -> None:
    """A long run crosses the sixty-minute ceiling mid-flight, so the refresh has to precede it.

    A refresh window at or past the lifetime would mint a replacement only after the old token
    had already expired — a 401 in the middle of a working stream. Asserted as an inequality
    with margin rather than against the literals, so the relationship is what is under test.
    """
    constants = microvms.session_constants()
    lifetime = constants["maxTokenLifetimeSeconds"]
    refresh = constants["defaultRefreshAfterSeconds"]
    assert lifetime == 3600
    assert 0 < refresh < lifetime
    # Half the lifetime of headroom, so one missed refresh is survivable.
    assert lifetime - refresh >= lifetime / 2


def test_the_default_agent_port_is_the_one_a_direct_session_lands_on() -> None:
    """One number, reachable two ways.

    A published constant that disagreed with the session's own default would send a harness
    validating the contract to a different port than the client uses.
    """
    assert (
        microvms.Session.direct("http://127.0.0.1:9000", "t").port
        == microvms.session_constants()["defaultAgentPort"]
    )
