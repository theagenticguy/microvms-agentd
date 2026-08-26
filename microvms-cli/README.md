# microvms-cli

The `microvm` binary: a working AWS Lambda MicroVM sandbox in one command.

```bash
cargo install microvms-cli
microvm --help
```

Every command answers with a JSON envelope, so the output is a parse rather than a screen
scrape, and the exit code is a rendering of the same error taxonomy the library exposes.
`microvm manifest` prints the machine-readable description of the whole command surface —
that is the entry point for an agent, ahead of any prose.

## Thin on purpose

This crate has six direct dependencies and no library target. There is no `reqwest`, no
`aws-*`, no `hyper` — it reaches AWS through `microvms-core` and through nothing else, and
`tests/thinness.rs` reads the manifest through `cargo metadata` and fails if the direct
dependency set is anything other than those six names. An allowlist rather than a denylist,
because a denylist is defeated by the one crate nobody thought to write down.

The absence of a library target is what makes "nothing a binding needs lives in the CLI" a
property instead of a request: there is no `src/lib.rs` to import, and a test fails if one
appears.

## Reading

Command reference and behavior:
[`docs/reference/cli.md`](https://github.com/theagenticguy/microvms-agentd/blob/main/docs/reference/cli.md).

## License

Apache-2.0
