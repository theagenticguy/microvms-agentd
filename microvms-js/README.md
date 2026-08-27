# @theagenticguy/microvms

Node bindings over `microvms-core`, the client library for AWS Lambda MicroVMs.

```bash
npm install @theagenticguy/microvms
```

A prebuilt native addon per platform, selected through `optionalDependencies`. No compiler
and no install-time download script runs on a consumer's machine.

Requires Node >= 22.13.

## Async-native

The core's async surface maps straight through: an exported `async` function runs on napi's
managed runtime and returns a real `Promise`, with an error rejecting it. Exec output and
stdin are handed across as `ReadableStream<Uint8Array>` rather than as async iterators a
consumer would have to adapt.

## The traps are in the types

The constraints the platform enforces at runtime are shapes this package refuses to
construct: a raw token cannot be passed where a session is expected, and a dollar amount is
never a bare float. Each planted bypass has a test that goes red if the door reopens.

## Reading

- [Documentation](https://theagenticguy.github.io/microvms-agentd/)
- [`docs/EMBEDDING.md`](https://github.com/theagenticguy/microvms-agentd/blob/main/docs/EMBEDDING.md)
- [`docs/TRUST.md`](https://github.com/theagenticguy/microvms-agentd/blob/main/docs/TRUST.md)

## License

Apache-2.0
