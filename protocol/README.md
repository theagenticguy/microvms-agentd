# microvms-protocol

The wire types shared by the `microvms-agentd` daemon and every client that talks to it:
the exec and file-transfer request and response shapes, `Phase`, `Outcome`, the SSE event
payloads, and the bootstrap contract.

This crate exists so that the daemon and the client cannot disagree. A field renamed here
fails to compile on both sides, which is earlier and cheaper than a generated schema
catching it in CI and far earlier than a consumer catching it at runtime.

`docs/schema.json` in the repository is derived from these same serde types, so the
published wire schema is a function of this crate rather than a document maintained beside
it.

## Naming

The package is `microvms-protocol` because `protocol` is taken on crates.io. Dependents
rename it back:

```toml
protocol = { package = "microvms-protocol", version = "0.1.0" }
```

## Reading

The wire contract is documented in [`docs/PROTOCOL.md`](https://github.com/theagenticguy/microvms-agentd/blob/main/docs/PROTOCOL.md),
which is hand-written and authoritative — it wins any disagreement with generated
documentation.

## License

Apache-2.0
