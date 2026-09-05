---
title: Configure the project file
description: Every key microvm.toml accepts, where run and doctor look for it, which source wins when a flag and the file disagree, and what the loader refuses.
editUrl: false
sidebar:
  order: 9
---

```toml
image = "ci-image"
exec = "pytest -q"
memory = 4096
region = "us-west-2"
egress = true
auto-resume = true
shell = true
max-idle-sec = 120
suspended-sec = 300
max-duration-sec = 7200
artifacts = ["dist/**", "*.log"]
log-group = "/aws/lambda-microvms/ci-builds"
log-stream = "ci-image"

[env]
RUST_LOG = "debug"
CI = "1"
```

Every knob in the file already exists as a `run` flag. The file adds no capability, only persistence, so `microvm run` in a configured project needs zero flags. At the end of this page you will have a `microvm.toml` that pins your project's launch, validated by `doctor`, and you will know which source won for each knob.

## 1. Where it is read

`run` and `doctor` look for `./microvm.toml` beside the invocation when `--config` is not given. `--config <PATH>` reads that file instead, and its absence is `ERR_CONFIG`, because a path you typed and got wrong must not silently become "no config"; the implicit default's absence means a project configured by flags, which is not an error. `--no-config` ignores any `microvm.toml`, even a malformed one, so flags and built-in defaults apply. `build` reads no config file, which is why `build --log-stream` requires `build --log-group` on the command line.

Field names are the flag names, so the file reads like the command line it replaces.

## 2. Every key

| Key                | The flag it persists   | Notes                                                                                                                                            |
| ------------------ | ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| `image`            | `run --image`          | Launch this existing image instead of building one. An ARN or a name.                                                                            |
| `binary`           | `run [BINARY]`         | The daemon binary to bake in. A relative path resolves against the config file's directory, never the process cwd.                              |
| `exec`             | `run --exec`           | The shell command to run in the VM.                                                                                                              |
| `memory`           | `run --memory`         | Baseline MiB. Validated against the same closed set as the flag: `512`, `1024`, `2048`, `4096`, `8192`.                                          |
| `region`           | `run --region`         | An unlisted region is refused at load with the remedy named: pass `--unlisted-region` on the command line.                                       |
| `egress`           | `run --egress`         | Give the VM outbound network.                                                                                                                    |
| `shell`            | `run --shell`          | Launch shell-capable, so `microvm shell` can attach later.                                                                                       |
| `auto-resume`      | `run --auto-resume`    | Let the platform resume a suspended VM on an incoming request.                                                                                   |
| `max-idle-sec`     | `run --max-idle-sec`   | Suspend after this much inbound-traffic idleness.                                                                                                |
| `suspended-sec`    | `run --suspended-sec`  | Terminate after this long suspended.                                                                                                             |
| `max-duration-sec` | `run --max-duration-sec` | Hard ceiling on the VM's life. Refused outside 1 through 28800, eight hours being the platform's ceiling.                                       |
| `[env]`            | `run --launch-env`     | The launch environment, as a table. Merged per key, with a `--launch-env` pair winning on a shared key. A key containing `=` or an empty key is refused. |
| `artifacts`        | none                   | Globs for `run <DIR>`: which files to bring back from the VM's synced working directory. Deliberately no flag spelling. Each glob must compile.   |
| `log-group`        | `run --log-group`      | The CloudWatch log group build logs go to. Validated against the platform's group-name shape; a colon usually means an ARN was pasted.            |
| `log-stream`       | `run --log-stream`     | A stream-name prefix inside `log-group`, which it requires. Capped at 495 characters and refused when it carries `:` or `*`.                     |

`binary` resolves relative to the file because `--config /repo/microvm.toml` from another directory is the flag's flagship case, and a `target/agentd` resolved against wherever the caller stands is either a miss or a different binary that happens to share the name. Two Windows path shapes that mean two things at once, a rooted path with no drive and a drive with no root, are refused rather than guessed.

## 3. Which source wins

A typed flag beats the file, and the file beats the built-in default. "Typed" is read off the parse rather than off the value, so `--memory 2048` overrides a file that says `4096` even though 2048 is also the default. The merge happens in one place and its outcome is on the `run` envelope as `resolvedConfig`: each knob's winning value and the source it came from, `flag`, `config`, `env`, or `default`. `env` appears only on the region, the one knob whose chain continues past the file into `$AWS_REGION` and `$AWS_DEFAULT_REGION`. `configPath` names the file that was read.

The `[env]` table merges per key, so a project pinning `RUST_LOG` is not discarded because you passed `--launch-env CI=1`; the flag wins its own key and the rest of the table survives. One pairing rule: a typed `BINARY` positional with no typed `--image` suppresses the file's `image`, because `run` builds exactly when the merged image is absent, and a file that silently won that pair would run your tests against a stale pinned image. A directory positional does not suppress it, because sync mode launches and the pinned image is exactly what `run .` wants.

## 4. What the loader refuses

Unknown keys are refused by name. A typo silently ignored is a config you believe is applied and is not; `memroy = 4096` launching a 2 GB VM is the failure this closes. A value outside the matching flag's domain is refused with the flag's own vocabulary, so `memory = 1500` cannot load for the same reason `--memory 1500` cannot parse. Every domain violation is reported at once rather than first-wins, because the file arrives as a unit.

A refused file is `ERR_CONFIG` (exit 15), its own row rather than `ERR_INVALID_ARG`, because the remedies differ: an invalid argument is fixed by editing the command line, and a broken config file is fixed by editing, or `--no-config` bypassing, a file the invocation may never have named. The refusal is local, before any billable call.

## 5. Validate it with doctor

```bash
microvm doctor --config microvm.toml
```

`doctor` validates the file through the same loader `run` uses, and the config check is its first line: fatal on a broken file, an advisory pass on an absent one. The two commands cannot disagree about a file, because there is one loader.

## 6. The shipped examples

Each example directory carries a `microvm.toml` that parses through the real loader, and a test pins that the coding-agents file still does. [coding-agents-on-bedrock](https://github.com/theagenticguy/microvms-agentd/blob/main/examples/coding-agents-on-bedrock/microvm.toml) pins `memory = 1024` with the sizing rule in its comments; [code-server-remote-dev](https://github.com/theagenticguy/microvms-agentd/blob/main/examples/code-server-remote-dev/microvm.toml) sets `egress`, `shell`, `auto-resume`, `max-idle-sec`, and an `[env]` table; [s3-prefetch-at-build](https://github.com/theagenticguy/microvms-agentd/blob/main/examples/s3-prefetch-at-build/microvm.toml) pins `egress = false` to prove a point. [Run a project through a VM](/learn/tutorial/run-a-project/) is where `artifacts` earns its place, and [`run`](/reference/commands/run/) lists every flag the file persists.
