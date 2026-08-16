# SPDX-License-Identifier: Apache-2.0
"""The stub against the module it describes, read a different way than the generator reads it.

`mise run stubs:check` regenerates and compares bytes, which catches the Rust surface moving
without the artifact following. It cannot catch the case where the *generator* is wrong,
because the generator is both sides of that comparison. These tests are the independent
reader: they parse `microvms.pyi` with `ast` — never importing it, never trusting the
generator's own bookkeeping — and compare what it declares against what `import microvms`
actually exposes.

That is the same argument `scripts/check-model-drift.py` makes about a self-comparison: a check
whose two sides come from one source cannot fail. So the assertions below deliberately do not
call the generator, and one of them (`test_no_getattr_escape_hatch`) exists to fail if a
future maturin reintroduces the line that made the whole stub unable to fail.

Nothing here talks to AWS, and nothing here needs the stub to be *installed* — the file is
read out of the source tree, which is also the copy the drift gate governs.
"""

from __future__ import annotations

import ast
from pathlib import Path

import pytest

import microvms

STUB = Path(__file__).resolve().parent.parent / "microvms.pyi"


@pytest.fixture(scope="module")
def tree() -> ast.Module:
    """The stub as a syntax tree.

    Parsing rather than importing, because a `.pyi` is not importable and a text search
    would confuse a class named in a docstring for a class the file declares — these
    docstrings quote type names constantly.
    """
    return ast.parse(STUB.read_text(), filename=str(STUB))


def declared_classes(tree: ast.Module) -> dict[str, ast.ClassDef]:
    return {node.name: node for node in tree.body if isinstance(node, ast.ClassDef)}


def declared_functions(tree: ast.Module) -> set[str]:
    return {
        node.name
        for node in tree.body
        if isinstance(node, ast.FunctionDef | ast.AsyncFunctionDef)
    }


def runtime_classes() -> dict[str, type]:
    return {
        name: value
        for name in dir(microvms)
        if isinstance(value := getattr(microvms, name), type)
    }


# -- the stub exists and ships -------------------------------------------------


def test_stub_is_committed() -> None:
    """The artifact is in the tree, which is what the drift gate has to have something to check."""
    assert STUB.is_file(), f"{STUB} is missing; run `mise run stubs`"


def test_py_typed_marker_exists() -> None:
    """PEP 561: without this file a checker ignores the stub entirely, however good it is."""
    marker = STUB.parent / "py.typed"
    assert marker.is_file(), (
        f"{marker} is missing; a checker will not read the stub without it"
    )


# -- the stub describes the real surface ---------------------------------------


def test_every_runtime_class_is_declared(tree: ast.Module) -> None:
    """No class reachable from `import microvms` is missing from the stub.

    This is the assertion that goes red when someone adds a `#[pyclass]` and does not
    regenerate — from the other direction than the drift gate, and without running the
    generator.
    """
    missing = sorted(set(runtime_classes()) - set(declared_classes(tree)))
    assert not missing, (
        f"classes the module exposes but the stub does not declare: {missing}"
    )


def test_no_class_is_invented(tree: ast.Module) -> None:
    """And nothing in the stub describes a class that does not exist.

    The dangerous direction: a caller's checker approving `microvms.Whatever()` because a
    stale stub still lists it is a green editor over code that raises `AttributeError`.
    """
    invented = sorted(set(declared_classes(tree)) - set(runtime_classes()))
    assert not invented, (
        f"classes the stub declares but the module does not expose: {invented}"
    )


def test_every_runtime_function_is_declared(tree: ast.Module) -> None:
    """The module-level functions, same two directions in one assertion."""
    runtime = {
        name
        for name in dir(microvms)
        if callable(getattr(microvms, name))
        and not isinstance(getattr(microvms, name), type)
    }
    assert runtime == declared_functions(tree)


@pytest.mark.parametrize(
    "name",
    [
        "Amount",
        "BaseImage",
        "BuildHookTimeout",
        "CostReport",
        "Duration",
        "EstimatedUsd",
        "ExecHandle",
        "Region",
        "RunHookTimeout",
        "Sandbox",
        "Session",
        "SizeClass",
    ],
)
def test_named_class_members_match(tree: ast.Module, name: str) -> None:
    """Every method and property the stub gives a class is really on that class.

    Not the reverse direction: pyo3 gives every class dunders the stub does not restate,
    and asserting equality would make this a test of pyo3's `__getstate__` rather than of
    the stub. What matters for a caller is that nothing the stub *promises* is absent —
    that is the failure that makes a checker approve an `AttributeError`.
    """
    runtime = getattr(microvms, name)
    declared = {
        node.name
        for node in declared_classes(tree)[name].body
        if isinstance(node, ast.FunctionDef | ast.AsyncFunctionDef)
    }
    absent = sorted(member for member in declared if not hasattr(runtime, member))
    assert not absent, f"{name} members the stub promises but the class lacks: {absent}"


# -- the exception hierarchy, which pyo3 does not introspect -------------------


def test_every_exception_is_declared(tree: ast.Module) -> None:
    """All 14 `create_exception!` types are in the stub.

    They are the part maturin cannot see — no `#[pyclass]` macro, so no introspection
    record — so they are appended by a second stage of the generator. A stage that silently
    produced nothing would leave `except MicrovmError` unresolvable for every caller, and
    the byte comparison in `stubs:check` would happily agree with itself about it.
    """
    runtime = {
        name
        for name, value in runtime_classes().items()
        if issubclass(value, BaseException)
    }
    assert len(runtime) == 14, (
        f"expected the 14-member hierarchy, found {sorted(runtime)}"
    )
    assert runtime <= set(declared_classes(tree))


def test_exception_bases_match_runtime(tree: ast.Module) -> None:
    """And each one inherits in the stub from what it inherits from at runtime.

    `except MicrovmError` catching everything is the contract `errors.rs` states; a stub
    that flattened the hierarchy would let a checker approve an `except` clause that misses.
    """
    classes = declared_classes(tree)
    for name, value in runtime_classes().items():
        if not issubclass(value, BaseException):
            continue
        bases = [base.id for base in classes[name].bases if isinstance(base, ast.Name)]
        assert bases == [value.__bases__[0].__name__], (
            f"{name} base differs from runtime"
        )


def test_base_exception_declares_the_four_attributes(tree: ast.Module) -> None:
    """`code`, `kind`, `wire_kind`, `retryable` are annotated on `MicrovmError`.

    `to_py_err` attaches them with `setattr`, so nothing about the type reveals them and
    nothing generated them. They are the entire reason `errors.rs` says nobody should parse
    a message — a caller who cannot see them in a checker goes back to parsing messages.
    """
    annotated = {
        node.target.id
        for node in declared_classes(tree)["MicrovmError"].body
        if isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name)
    }
    assert {"code", "kind", "wire_kind", "retryable"} <= annotated


# -- the stub can fail --------------------------------------------------------


def test_no_getattr_escape_hatch() -> None:
    """No module-level `__getattr__`, because it makes every other assertion vacuous.

    maturin appends `def __getattr__(name: str) -> Incomplete: ...` to its generated stubs,
    and the generator strips it. Measured with the line present: `from microvms import
    CompletelyMadeUpName` type-checks clean, because the hatch answers every unknown name
    with `Incomplete`. A `py.typed` package that answers every question with `Any` is worse
    than an untyped one — it promises a checker that silence means correctness.
    """
    assert "def __getattr__" not in STUB.read_text()


def test_trap_closures_are_visible_to_a_checker(tree: ast.Module) -> None:
    """The three closures a checker can see, asserted on the stub's own text.

    Runtime tests for these already exist in `test_smoke.py` and `test_cost.py`; what is new
    is that a *type checker* now refuses them, which is the difference between finding the
    mistake when the code runs and finding it while it is being written.
    """
    classes = declared_classes(tree)

    # `EstimatedUsd.amount` answers a string, so `float(usd)` has nothing to consume and
    # arithmetic has to go through an explicit `Decimal(...)` a reviewer can see.
    amount = next(
        node
        for node in classes["EstimatedUsd"].body
        if isinstance(node, ast.FunctionDef) and node.name == "amount"
    )
    assert isinstance(amount.returns, ast.Name) and amount.returns.id == "str"

    # `EstimatedUsd` has no numeric door at all.
    dunders = {
        node.name
        for node in classes["EstimatedUsd"].body
        if isinstance(node, ast.FunctionDef)
    }
    assert not dunders & {"__float__", "__int__", "__index__", "__add__"}

    # `Duration` has no `__new__`, so the provenance cannot be omitted.
    duration = {
        node.name
        for node in classes["Duration"].body
        if isinstance(node, ast.FunctionDef)
    }
    assert "__new__" not in duration
    assert {"measured", "projected"} <= duration

    # The two hook timeouts are unrelated classes rather than two ints, so the
    # transposition is a type error before pyo3's argument conversion sees it.
    for timeout in ("RunHookTimeout", "BuildHookTimeout"):
        assert not classes[timeout].bases, (
            f"{timeout} should not inherit from the other family"
        )
