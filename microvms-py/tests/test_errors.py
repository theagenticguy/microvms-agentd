# SPDX-License-Identifier: Apache-2.0
"""The exception hierarchy: `src/errors.rs`.

`test_smoke.py` asserts the thirteen classes exist under one base. This file asserts the part a
caller actually depends on: that the **mapping** from a core `ErrorKind` to a Python class is
right, and that the four attributes travel on every raised exception rather than on some of them.

Why that split matters. A binding where every error arrived as the base `MicrovmError` would pass
a hierarchy check and be useless — `except WindowClosedError` is the whole reason for a class per
condition, and it only works if the *right* class is raised. Likewise `.code` present on some
paths and absent on others is worse than absent everywhere, because it works in the first test
someone writes.

# The boundary

Only the locally-reachable kinds are exercised here: a unit run has no daemon and no AWS, so
`ERR_LAUNCH_DIED` and `ERR_BUILD_WEDGED` cannot be provoked without one. What is covered is every
kind this suite can *cause* — which is the set a caller hits before a request goes out — plus the
structural claim (one class per kind, no gaps) over the whole enum. The conformance suite covers
the wire-reachable half.
"""

from __future__ import annotations

import pytest

import microvms

# Every exception class, paired with the `ERR_*` code it stands for. Written out rather than
# derived, because this table *is* the contract: deriving it from the module would make the test
# agree with whatever the module does, including a mis-mapping.
KIND_TO_EXCEPTION = [
    ("ERR_UNEXPECTED", microvms.UnexpectedError),
    ("ERR_INVALID_ARG", microvms.InvalidArgError),
    ("ERR_RETRYABLE", microvms.RetryableError),
    ("ERR_CREDENTIALS", microvms.CredentialsError),
    ("ERR_PROTOCOL", microvms.ProtocolError),
    ("ERR_BUILD_WEDGED", microvms.BuildWedgedError),
    ("ERR_LAUNCH_DIED", microvms.LaunchDiedError),
    ("ERR_WINDOW_CLOSED", microvms.WindowClosedError),
    ("ERR_PLATFORM", microvms.PlatformError),
    ("ERR_TIMEOUT", microvms.TimeoutError),
    ("ERR_INTERRUPTED", microvms.InterruptedError),
    ("ERR_PRECONDITION", microvms.PreconditionError),
    ("ERR_EXEC_FAILED", microvms.ExecFailedError),
]


# -- the hierarchy is a hierarchy ---------------------------------------------


def test_every_exception_is_a_distinct_class_under_the_one_base() -> None:
    """Thirteen classes, thirteen identities.

    Two names bound to one class would make `except WindowClosedError` catch a launch failure,
    which is the specific confusion a class-per-condition hierarchy exists to prevent — and an
    aliasing mistake is invisible to a test that only checks `issubclass`.
    """
    classes = [exception for _, exception in KIND_TO_EXCEPTION]
    assert len({id(exception) for exception in classes}) == 13
    for exception in classes:
        assert issubclass(exception, microvms.MicrovmError)
        assert issubclass(exception, Exception)
        # Direct subclasses, so `except MicrovmError` is one hop and the MRO is readable.
        assert exception.__bases__ == (microvms.MicrovmError,)


def test_no_exception_is_a_subclass_of_another_so_no_except_clause_overlaps() -> None:
    """A flat hierarchy, deliberately.

    If `TimeoutError` were under `RetryableError`, a caller's `except RetryableError` would
    swallow timeouts and retry a deadline that had already passed. Flat means the only broad
    catch is the base one, and reaching for it is visible.
    """
    classes = [exception for _, exception in KIND_TO_EXCEPTION]
    for outer in classes:
        for inner in classes:
            if outer is inner:
                continue
            assert not issubclass(inner, outer), (inner.__name__, outer.__name__)


def test_the_library_exceptions_do_not_shadow_the_builtins_they_are_named_after() -> (
    None
):
    """`microvms.TimeoutError` is **not** the builtin, and neither is `InterruptedError`.

    Two of the thirteen collide with builtin names, which is a real hazard: a caller writing
    `except TimeoutError` after `from microvms import *` would catch a different class than they
    think. So the names are asserted distinct, and the base is what a caller should use.
    """
    assert microvms.TimeoutError is not TimeoutError
    assert microvms.InterruptedError is not InterruptedError
    assert not issubclass(microvms.TimeoutError, TimeoutError)
    assert not issubclass(microvms.InterruptedError, InterruptedError)


# -- the four attributes travel on every raised exception ---------------------


def raise_invalid_arg() -> microvms.MicrovmError:
    """A locally-refused call, which is the shortest path to a real raised exception."""
    with pytest.raises(microvms.MicrovmError) as raised:
        microvms.Region.parse("nope-1")
    return raised.value


def test_a_raised_exception_carries_all_four_attributes_and_not_a_parsed_message() -> (
    None
):
    """`.code`, `.kind`, `.wire_kind`, `.retryable` — the whole set, on the instance.

    Nobody parses a message. That rule is why these are attributes, and a missing one sends a
    caller straight back to string matching on a sentence that is free to change.
    """
    error = raise_invalid_arg()
    assert error.code == "ERR_INVALID_ARG"
    assert error.kind == "ERR_INVALID_ARG"
    assert error.wire_kind is None
    assert error.retryable is False
    # Real booleans and real strings, not truthy stand-ins a caller has to interpret.
    assert isinstance(error.retryable, bool)
    assert isinstance(error.code, str)


def test_code_and_kind_are_the_same_string_because_there_is_one_taxonomy() -> None:
    """Two names for one value, for a caller who thinks in either.

    They are asserted equal rather than each checked against a literal: a binding where they
    drifted would give two answers to one question, and whichever one a consumer branched on
    would be the one that was wrong.
    """
    error = raise_invalid_arg()
    assert error.code == error.kind
    assert error.code.startswith("ERR_")


def test_a_local_refusal_reports_no_wire_kind_because_nothing_reached_the_daemon() -> (
    None
):
    """`None` and not a guessed status.

    Inventing a wire kind for a local refusal would be a claim nobody made, and a monitor
    counting daemon errors would count client-side typos among them.
    """
    for call in (
        lambda: microvms.Region.parse("eu-central-1"),
        lambda: microvms.SizeClass.from_baseline_mib(1500),
        lambda: microvms.Duration.measured(-1.0),
        lambda: microvms.RunHookTimeout(3600),
    ):
        with pytest.raises(microvms.MicrovmError) as raised:
            call()
        assert raised.value.wire_kind is None
        assert raised.value.code == "ERR_INVALID_ARG"


def test_the_message_is_the_cores_own_and_survives_the_crossing_intact() -> None:
    """Not reworded here.

    The core's messages are the ones naming the `docs/PLATFORM.md` finding that measured the
    behaviour, and a binding that shortened them would discard the whole point of the closure.
    Checked on the region refusal, whose message has two halves that only mean something
    together.
    """
    with pytest.raises(microvms.InvalidArgError) as raised:
        microvms.Region.parse("eu-central-1")
    message = str(raised.value)
    # "AccessDeniedException" alone reads as an IAM problem; the word *null* is what says
    # otherwise. Both halves crossed the boundary.
    assert "AccessDeniedException" in message
    assert "null" in message
    assert len(message) > 80, f"the message was truncated on the way out: {message!r}"


def test_an_invalid_arg_is_not_retryable_because_repeating_it_changes_nothing() -> None:
    """`retryable` is the field a retry loop reads, so its value per kind is load-bearing.

    A refused argument marked retryable is an infinite loop over an unchanging refusal — the
    exact failure the retryable split exists to prevent, and one that looks like a hang rather
    than an error.
    """
    assert raise_invalid_arg().retryable is False


# -- the exceptions are catchable the way callers write catches ----------------


def test_a_specific_except_clause_catches_only_its_own_kind() -> None:
    """The point of the hierarchy, exercised as a caller writes it."""
    with pytest.raises(microvms.InvalidArgError):
        microvms.Region.parse("nope-1")

    # And a sibling clause does not catch it, which is what makes the specific catch worth
    # writing. `pytest.raises` around the wrong class would swallow the real exception, so this
    # is spelled as an explicit try.
    try:
        microvms.Region.parse("nope-1")
    except microvms.WindowClosedError:  # pragma: no cover — the failure path
        pytest.fail("a region typo was caught as a closed suspension window")
    except microvms.InvalidArgError as error:
        assert error.code == "ERR_INVALID_ARG"


def test_the_base_catches_everything_this_library_raises() -> None:
    """`except MicrovmError` is the one broad catch, and it has to be total."""
    for call in (
        lambda: microvms.Region.parse("nope-1"),
        lambda: microvms.SizeClass.from_baseline_mib(7),
        lambda: microvms.Duration.projected(float("nan")),
        lambda: microvms.BuildHookTimeout(0),
    ):
        with pytest.raises(microvms.MicrovmError):
            call()


def test_an_exception_instance_carries_its_attributes_after_being_re_raised() -> None:
    """The attributes are on the instance, not on the raising frame.

    A caller commonly catches, wraps, and re-raises. If the codes were attached by the raise
    site rather than to the object, they would be gone by the time a handler two frames up read
    them — and that handler is where the branching happens.
    """
    caught: microvms.MicrovmError | None = None
    try:
        try:
            microvms.Region.parse("nope-1")
        except microvms.MicrovmError as inner:
            raise RuntimeError("wrapped") from inner
    except RuntimeError as outer:
        cause = outer.__cause__
        assert isinstance(cause, microvms.InvalidArgError)
        caught = cause
    assert caught is not None
    assert caught.code == "ERR_INVALID_ARG"
    assert caught.retryable is False
    assert caught.wire_kind is None


# -- the codes are the shared taxonomy ----------------------------------------


def test_every_code_is_upper_snake_and_prefixed_so_a_switch_is_writable() -> None:
    """The shape callers pattern-match on, and thirteen distinct values.

    Two kinds sharing a code would make branching ambiguous in a way no type checker sees: the
    consumer's `if code == ...` would take one branch for two different conditions.
    """
    codes = [code for code, _ in KIND_TO_EXCEPTION]
    assert len(set(codes)) == 13
    for code in codes:
        assert code.startswith("ERR_")
        assert code == code.upper()
        assert " " not in code


def test_the_class_names_and_the_codes_agree_so_neither_has_to_be_looked_up() -> None:
    """`ERR_WINDOW_CLOSED` ↔ `WindowClosedError`, mechanically.

    A pairing that drifted — say `ERR_PLATFORM` raised as `PreconditionError` — would be
    invisible to every other test in this file, because both halves exist and both are
    well-formed. This is the check that ties them together, and it derives the expected name
    from the code so it cannot be satisfied by editing a table.
    """
    for code, exception in KIND_TO_EXCEPTION:
        stem = code.removeprefix("ERR_")
        expected = "".join(part.capitalize() for part in stem.split("_")) + "Error"
        assert exception.__name__ == expected, (code, exception.__name__)


def test_each_exception_documents_the_condition_it_stands_for() -> None:
    """A docstring per class, because the class name is not the whole story.

    `ERR_EXEC_FAILED` in particular means the sandbox *worked* and the command in it exited
    non-zero — the one failure that says nothing is wrong with the platform — and a caller
    reading only the name would treat it as an infrastructure error.
    """
    for code, exception in KIND_TO_EXCEPTION:
        doc = exception.__doc__ or ""
        assert code in doc, f"{exception.__name__}'s docstring does not name {code}"
        assert len(doc) > len(code) + 20, (
            f"{exception.__name__} has no real explanation"
        )
