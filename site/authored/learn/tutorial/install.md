---
title: Install the CLI
description: Install the microvm binary with cargo binstall or from crates.io, learn how the daemon reaches your images, add a library if you want one, and confirm the install with microvm manifest.
editUrl: false
sidebar:
  order: 1
---

One artifact gets you running: the `microvm` CLI. The `agentd` daemon that gets baked into your VM images is the CLI's own component, and the CLI provisions it for you.

At the end of this page you will have `microvm` on your `PATH`, answering `microvm manifest` with no credentials, and you will know where the daemon binary comes from and which library to add if you want the same lifecycle from code.

## 1. Install the CLI

Prebuilt from the release assets, in seconds, or compiled from crates.io, in minutes:

```bash
cargo binstall microvms-cli           # installs the `microvm` binary, prebuilt
cargo install microvms-cli --locked   # the same binary, compiled locally
```

Release assets cover Linux (x86_64 and aarch64, glibc 2.17 floor), macOS (arm64 and x86_64), and Windows (x64), each with a signed build-provenance attestation beside it. There is deliberately no Homebrew formula: a tap is a second repository with its own release cadence to maintain, and binstall plus these assets already cover macOS.

## 2. Confirm it

```bash
microvm manifest
```

```json
{
  "apiVersion": "1",
  "data": {
    "apiVersion": "1",
    "cli": "microvm",
    "commands": [
```

`microvm manifest` is the liveness check. It answers on a machine with no AWS credentials, no region, and no network, so an envelope back means the install is good. Its output is always JSON, whatever `--json` says, because the manifest is the command a tool reads to learn the surface without parsing help text: every command, every flag with its type and default and closed set of choices, every response type, and every exit code. `microvm manifest --dense` prints the same surface as one line per command.

## 3. Where the daemon comes from

Lambda MicroVMs are ARM64-only, so `agentd` is a static `aarch64-unknown-linux-musl` build that bakes into any base image with no interpreter and no dynamic loader. You do not download it. `run`, `build`, and `quickstart` provision the release asset for their own version, verify it (through `gh attestation verify` when `gh` is on your `PATH`, through the release's `SHA256SUMS` otherwise), and cache it under `~/.microvm`.

If you want to manage the binary yourself, for a custom build or an airgapped machine, fetch and verify it the same way the CLI does, then pass it as the positional argument or as `$MICROVM_AGENTD`:

```bash
gh release download --repo theagenticguy/microvms-agentd --pattern agentd
gh attestation verify agentd --repo theagenticguy/microvms-agentd
chmod +x agentd
```

`microvm doctor --binary ./agentd` reads the ELF header and refuses a host-architecture binary before it costs you a build cycle. `agentd` is deliberately absent from crates.io: the daemon reaches a consumer as a binary inside a task image, and the GitHub release is that channel.

## 4. The libraries

The same lifecycle, with the same defaults and the same guardrails, is available from code:

```bash
cargo add microvms-core                   # Rust; API reference on docs.rs
uv add microvms                           # Python >= 3.9; or pip install microvms
npm install @theagenticguy/microvms       # Node >= 22.13
```

The Python package ships abi3 wheels for Linux, macOS, and Windows, so one wheel per platform covers CPython 3.9 and everything after it, with a `py.typed` marker and a generated stub beside the extension. The npm package carries every platform binary in one package, with `index.d.ts` beside the addon. All three registries are published through OIDC trusted publishing, so no long-lived registry token exists anywhere. [Drive it from code](/learn/tutorial/from-code/) shows the surface, and [Public API](/reference/public-api/) lists it.

## 5. Or build it from a clone

Contributors, and anyone who wants the binaries from a specific commit, build them instead. `mise` is the command surface for the repository and installs the toolchain from `mise.toml`:

```bash
git clone https://github.com/theagenticguy/microvms-agentd.git
cd microvms-agentd
mise install               # the toolchain
mise run install           # the git hooks
mise run build             # cross-compiles the daemon to aarch64-unknown-linux-musl
mise run install:cli       # installs the `microvm` binary from the working tree
```

`mise run check` is the definition of done for a change and runs offline, free. The tiers that talk to real AWS are separate and billable; [Run the live suite](/learn/operations/run-the-live-suite/) covers them.

Next: [run your first command in a MicroVM](/learn/tutorial/first-run/).
