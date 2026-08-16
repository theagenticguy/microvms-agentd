# SPDX-License-Identifier: Apache-2.0
"""A typed consumer, which exists so a type checker has something to be right about.

`mise run stubs:check` proves `microvms.pyi` still describes the Rust surface. It cannot
prove the stub is *useful* — a file full of `Any` would pass that gate forever. This module
is the other half: every annotation below is one a checker resolves through the stub, so a
stub that regressed to `Any` or lost a class would stop checking here.

# Why annotations rather than assertions

Nothing here runs against AWS, and most of it does not run at all: `main()` is guarded so
the module is import-safe, and the sandbox lifecycle is written but never entered. What is
being verified is what `ty` can conclude *without* executing anything, because that is what
a caller in an editor gets. `microvms-py/tests/test_stubs.py` is the executable half, and it
is the one that fails when the two disagree.

# The four closures, restated as things a checker can see

The crate's own module docs list four mistakes the bindings refuse to make available. Three
of the four are now visible to a checker rather than only at runtime:

* `EstimatedUsd.amount` is typed `str`, so `total_usd` below could not have been annotated
  `float` — the trap closure is a type error at edit time, not a `TypeError` at run time.
* `Duration` has no `__new__` in the stub, so `Duration(3600.0)` does not check.
* `RunHookTimeout` and `BuildHookTimeout` are distinct classes in the stub, so the
  transposition is caught before pyo3's argument conversion ever sees it.

The fourth is `client_token`, which is closed by absence in the core and therefore has
nothing to annotate.
"""

from __future__ import annotations

from microvms import (
    BuildHookTimeout,
    CostReport,
    Duration,
    EstimatedUsd,
    MicrovmError,
    Region,
    RunHookTimeout,
    Sandbox,
    SizeClass,
    Total,
    WindowClosedError,
    core_version,
    estimate_run,
    run_report,
)


def pick_region(configured: str | None) -> Region:
    """A region from a config string, through the refusal rather than around it.

    The parameter is `str | None` because that is what a config file hands you; the return
    is a `Region` because that is the only thing the rest of this module accepts. The
    narrowing happens in exactly one place, which is the point of TRAP-6 being a type.
    """
    if configured is None:
        return Region.us_east_1()
    return Region.parse(configured)


def size_for(baseline_mib: int) -> SizeClass:
    """The class a `minimumMemoryInMiB` selects. Refuses off-table figures (TRAP-10)."""
    return SizeClass.from_baseline_mib(baseline_mib)


def plan_cost(size: SizeClass, seconds: float) -> CostReport:
    """A projection, before anything is launched."""
    return estimate_run(size, running_seconds=seconds)


def measured_cost(size: SizeClass, ran_for: float) -> CostReport:
    """A report over a phase a clock actually timed.

    `Duration.measured` rather than `Duration(...)`: the stub has no `__new__` for
    `Duration`, so the provenance cannot be omitted here any more than it can in Rust.
    """
    return run_report(size, running=Duration.measured(ran_for))


def render_total(report: CostReport) -> str:
    """The figure as a string, which is the only thing the money types will give up.

    `total.floor` is an `EstimatedUsd` and `.amount` is a `str`. Annotating `amount: str`
    is what makes this function a test: if the stub ever typed it as a float, or as `Any`
    coming back from an untyped module, the annotation below would stop agreeing with it.
    """
    total: Total = report.total
    floor: EstimatedUsd = total.floor
    amount: str = floor.amount
    if total.is_lower_bound:
        return f"at least {amount}"
    return amount


def hook_timeouts() -> tuple[RunHookTimeout, BuildHookTimeout]:
    """Both families, each as its own type.

    The two ceilings are 60x apart (60s and 3600s), and the transposition is the mistake
    the split classes exist to prevent. A checker reading this stub refuses
    `RunHookTimeout(3600)` at the call site of anything expecting a build timeout, because
    the classes are unrelated rather than two ints.
    """
    return RunHookTimeout(30), BuildHookTimeout(1800)


def describe_client() -> str:
    """The core's version, which is what a `doctor` command should report."""
    return core_version()


def launch(region: Region, size: SizeClass) -> str:
    """The lifecycle, written for the checker rather than for a run.

    Never called by anything in this repo's gates: it resolves credentials in `Sandbox()`
    and would create a real MicroVM. It is here because the lifecycle surface is most of
    what a caller touches, and a stub that typed `run()` or `.session` wrongly would only
    be caught by code that names them.
    """
    run_timeout, build_timeout = hook_timeouts()
    with Sandbox(region) as sandbox:
        image = sandbox.build_image(
            name="example",
            binary=b"",
            code_artifact_uri="s3://example/artifact.zip",
            build_role_arn="arn:aws:iam::123456789012:role/example",
            size=size,
            run_hook_timeout=run_timeout,
            build_hook_timeout=build_timeout,
        )
        # `image_identifier` is the ARN string, which `Image.identifier` is the typed way
        # to reach. A checker refuses `run(image_identifier=image)` here, which is the kind
        # of mistake that used to survive until a control-plane rejection.
        sandbox.run(image_identifier=image.identifier)
        try:
            sandbox.resume()
        except WindowClosedError as closed:
            # `.code` and `.retryable` are declared on the base class in the stub, so a
            # caller can read them off any raised exception instead of parsing a message.
            return f"{closed.code} retryable={closed.retryable}"
        except MicrovmError as error:
            return f"{error.kind}: {error}"
        endpoint = sandbox.endpoint
        return endpoint if endpoint is not None else "no endpoint"


def main() -> None:
    """Everything that is safe to run without AWS: the value types and the cost surface."""
    region = pick_region(None)
    size = size_for(2048)
    print(f"client {describe_client()} in {region.name} at {size.describe()}")
    print(f"planned: {render_total(plan_cost(size, 300.0))}")
    print(f"measured: {render_total(measured_cost(size, 287.4))}")


if __name__ == "__main__":
    main()
