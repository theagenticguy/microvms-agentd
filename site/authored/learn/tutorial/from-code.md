---
title: Drive it from code
description: The same lifecycle as a Rust crate, a Python package, and a Node package, with value types that refuse the platform's traps, shown through the typed example the repository ships.
editUrl: false
sidebar:
  order: 5
---

The `microvm` CLI is a thin layer over the `microvms-core` crate, and the Python and Node packages are thin bindings over the same crate. Whatever the CLI refuses locally, the libraries refuse too, and most of those refusals are visible to a type checker before anything runs.

At the end of this page you will know which package to add, what the typed surface refuses to construct, and you will have run the credential-free half of the Python example the repository ships.

## 1. Pick a library

```bash
cargo add microvms-core                   # Rust; API reference on docs.rs
uv add microvms                           # Python >= 3.9; or pip install microvms
npm install @theagenticguy/microvms       # Node >= 22.13
```

The Python stub and the Node `index.d.ts` are generated from the Rust source, never hand-written, and `mise run stubs:check` fails on any difference between the Python stub and the compiled module. [Public API](/reference/public-api/) lists the Rust surface the bindings wrap.

## 2. The traps are in the types

The constraints the platform enforces at runtime are shapes these packages refuse to construct. A dollar amount is a string a checker refuses to add, so it cannot be silently summed with a float. A `Duration` has no constructor that omits its provenance, so a projected figure cannot be mistaken for a measured one. The run-hook and build-hook timeouts are unrelated classes rather than two integers, so the two ceilings cannot be transposed. A raw token cannot be passed where a session is expected. Each planted bypass has a test that goes red if the door reopens.

## 3. Python, without credentials

`microvms-py/examples/typed_usage.py` is a typed consumer that exists so a checker has something to be right about. Its value and cost surface runs with no AWS account. These functions are quoted from it:

```python
from microvms import CostReport, Duration, EstimatedUsd, SizeClass, Total, estimate_run, run_report


def size_for(baseline_mib: int) -> SizeClass:
    """The class a `minimumMemoryInMiB` selects. Refuses off-table figures (TRAP-10)."""
    return SizeClass.from_baseline_mib(baseline_mib)


def plan_cost(size: SizeClass, seconds: float) -> CostReport:
    """A projection, before anything is launched."""
    return estimate_run(size, running_seconds=seconds)


def measured_cost(size: SizeClass, ran_for: float) -> CostReport:
    """A report over a phase a clock actually timed."""
    return run_report(size, running=Duration.measured(ran_for))


def render_total(report: CostReport) -> str:
    """The figure as a string, which is the only thing the money types will give up."""
    total: Total = report.total
    floor: EstimatedUsd = total.floor
    amount: str = floor.amount
    if total.is_lower_bound:
        return f"at least {amount}"
    return amount
```

`Duration.measured` rather than `Duration(...)`: the stub has no `__new__` for `Duration`, so the provenance cannot be omitted here any more than it can in Rust. `amount` is annotated `str` because that is what the stub says it is; if the stub ever typed it as a float, the annotation would stop agreeing with it. Run the example's `main()` with the package installed:

```bash
pip install microvms
python3 microvms-py/examples/typed_usage.py
```

## 4. Python, the lifecycle

The same file writes the lifecycle for the checker, and never runs it in any gate, because `Sandbox()` resolves credentials and would create a real MicroVM:

```python
def launch(region: Region, size: SizeClass) -> str:
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
        sandbox.run(image_identifier=image.identifier)
        try:
            sandbox.resume()
        except WindowClosedError as closed:
            return f"{closed.code} retryable={closed.retryable}"
        except MicrovmError as error:
            return f"{error.kind}: {error}"
        endpoint = sandbox.endpoint
        return endpoint if endpoint is not None else "no endpoint"
```

`image.identifier` is the ARN string; a checker refuses `run(image_identifier=image)`, which is the kind of mistake that used to survive until a control-plane rejection. `hook_timeouts()` in the same file returns `RunHookTimeout(30), BuildHookTimeout(1800)`, the two classes the transposition trap is closed by. Every raised exception carries `.code` and `.retryable` on the base class, so a caller reads them instead of parsing a message; the codes are the same `ERR_*` strings the CLI's envelope carries, listed at [Exit codes](/reference/exit-codes/).

## 5. Node

The napi-rs binding is async-native: an exported async function runs on napi's managed runtime and returns a real `Promise`, and exec output arrives as a `ReadableStream<Uint8Array>`. The smoke tests in `microvms-js/__test__/smoke.mjs` run with no AWS account, and this helper is quoted from them:

```js
import { Duration, SizeClass, runReport } from '../index.js';

function report() {
  return runReport(SizeClass.defaultClass(), {
    running: Duration.measured(3600),
    imageGb: 2,
    label: 'smoke',
  });
}
```

`imageGb` is what makes the build line appear, so this report is deliberately incomplete, and its `total` is a lower bound. The same file asserts that `Number(usd)`, `+usd`, `usd * 2`, and `JSON.stringify(usd)` never yield a bare number, because JavaScript coerces far more eagerly than Python does and each of those is a separate door.

One thing to know before reading an error: napi types the async path over its own closed status enum, so a custom code survives a synchronous throw and is collapsed to `GenericFailure` on a Promise rejection. Read `err.cause.message` for the `ERR_*` code and `err.cause.cause.message` for the fine-grained wire kind.

## 6. Rust

`microvms-core` is the crate the CLI is a thin layer over, with the control plane (image builds, launch, suspend, resume, teardown), the session plane (exec, streaming, file transfer, port forwarding), the cost engine, and the closed enums for regions and size classes. The API reference is on [docs.rs](https://docs.rs/microvms-core), and [Public API](/reference/public-api/) lists the types by name.

Whatever language you drive it from, the daemon's wire contract is the same: [Protocol](/internals/protocol/) is the reading, [wire schema](/reference/wire-schema/) is the JSON Schema the daemon also serves at `GET /v1/schema`, and [Embedding](/internals/embedding/) is what a harness client has to implement.

This is the last tutorial. The [operations pages](/learn/) answer the questions that come up once something is running.
