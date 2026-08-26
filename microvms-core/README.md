# microvms-core

The client library for AWS Lambda MicroVMs: the control plane, the in-VM session surface,
the cost engine, and the sizing model.

The type system carries every trap closure. A constraint the platform enforces at runtime —
a size class that does not exist, a region that does not offer the service, a token that
must not reach a log line — is a shape this crate refuses to construct rather than a check
a caller can forget. `docs/PLATFORM.md` records the measured findings each one closes,
with a date, a region, and an API version.

Money is `rust_decimal`, not `f64`. The ARM rates are figures like `0.0000276944`, and
summing a few thousand of them in binary floating point drifts toward a bill nobody can
reproduce.

## Position in the stack

```
microvms-cli ──▶ microvms-core ──▶ microvms-protocol
                       ▲
        microvms-py ───┴─── microvms-js
```

Nothing depends on the CLI. That direction is asserted mechanically by
`microvms-cli/tests/dependency_direction.rs` rather than documented and hoped for.

## Reading

- [`docs/TRUST.md`](https://github.com/theagenticguy/microvms-agentd/blob/main/docs/TRUST.md) — the threat model and the trust boundary
- [`docs/PLATFORM.md`](https://github.com/theagenticguy/microvms-agentd/blob/main/docs/PLATFORM.md) — measured platform behavior
- [`docs/EMBEDDING.md`](https://github.com/theagenticguy/microvms-agentd/blob/main/docs/EMBEDDING.md) — using this crate from a host application

## License

Apache-2.0
