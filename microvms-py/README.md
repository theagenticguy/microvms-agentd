# microvms

Python bindings over `microvms-core`, the client library for AWS Lambda MicroVMs.

```bash
pip install microvms
```

One wheel per platform, built `abi3-py39`, so a single artifact loads on CPython 3.9 and
newer rather than one wheel per interpreter version.

## Typed

The wheel ships `py.typed` and a generated `__init__.pyi` beside the extension, so a checker
reads the surface by the ordinary PEP 561 rules. The stub is generated from the compiled
module and compared against it in CI — a stale stub is worse than none, because it leaves a
caller confidently wrong with an editor approving a call that raises `AttributeError`.

## The traps are in the types

The constraints the platform enforces at runtime are shapes this package refuses to
construct: a raw token cannot be passed where a session is expected, a capability list
cannot be widened after the fact, and a dollar amount is never a bare float. Nine planted
bypasses each have a test that goes red if the door reopens.

## Reading

- [Documentation](https://theagenticguy.github.io/microvms-agentd/)
- [`docs/EMBEDDING.md`](https://github.com/theagenticguy/microvms-agentd/blob/main/docs/EMBEDDING.md)
- [`docs/TRUST.md`](https://github.com/theagenticguy/microvms-agentd/blob/main/docs/TRUST.md)

## License

Apache-2.0
