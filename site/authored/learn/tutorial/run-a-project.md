---
title: Run a project through a VM
description: Point run at a directory to pack it, upload it to /workspace, run a command there, and bring artifacts back; then bake the project's dependencies into the image with build --project.
editUrl: false
sidebar:
  order: 4
---

When the positional argument to `run` is a directory rather than a binary, `run` becomes a pack-run-collect round trip against an existing image. This tutorial runs a project's test command that way, brings its reports back, and then builds an image that already carries the project's dependencies.

At the end of this page your project's command will have run inside a VM with the tree at `/workspace`, its artifacts will be back on your disk, and the image will carry a dependency layer so a launch skips installation.

You need an image to launch, so [build and keep one](/learn/tutorial/first-run/) first.

## 1. The round trip

```bash
microvm run . --image ci-image --exec "make test"
```

A positional that names a directory switches `run` into sync mode. Sync mode launches an existing image, named by `--image` or by `image` in `microvm.toml`; a directory with nothing supplying an image is refused with `ERR_PRECONDITION` before any call. A positional that names a file is a daemon binary you manage yourself, and the two readings cannot collide, because a path is a directory or it is not.

## 2. What is packed and what stays home

The tree is packed locally in deterministic member order. `.git`, `target`, `node_modules`, and `.venv` are skipped whole; sockets, fifos, and devices are skipped individually; symlinks are preserved as links and never followed, so a link out of the project does not pull the rest of your disk into the archive.

The pack is budgeted against the daemon's own caps, 512 MiB of file bytes and its member limit, during the walk. An over-budget tree is `ERR_SYNC` (exit 16) naming the offending subtree, before any archive bytes are allocated and before any AWS call is made.

## 3. In the guest

The archive is uploaded to `/workspace` in the guest, and the exec runs with `/workspace` as its working directory. The daemon spawns execs with a minimal environment, so a command that expects `PATH` gets it from the `[env]` table in `microvm.toml` or from `--launch-env KEY=VALUE`.

The `run` envelope's `sync` key reports the workdir, the uploaded bytes, and the member count.

## 4. Bring artifacts back

Which members come back is declared in `microvm.toml` as `artifacts` globs. There is deliberately no flag spelling for a list this shape:

```toml
image = "ci-image"
exec = "make test"
artifacts = ["dist/**", "*.log"]
```

With that file beside the invocation, `microvm run .` needs no flags at all. A typed flag still wins over the file, and the file wins over the built-in default; the envelope's `resolvedConfig` names which source won for each knob. [Configure the project file](/learn/operations/configure-the-project-file/) lists every key.

Afterwards, members matching the globs are written into the local directory, including when the command failed, because a failing run's report is the artifact CI most wants. The command's own exit code stands: `ERR_EXEC_FAILED` (exit 13) says the sandbox worked and the command in it exited non-zero, which is a different sentence from "we never got a VM".

Only glob-matched regular-file members land, and never under `.git`. Symlinks, hardlinks, specials, unmatched members, anything attempting traversal outside the directory, and any `.git` path are skipped, because the returned archive is the VM's word, the VM is where untrusted work runs, and a workload-written `.git/hooks/pre-commit` would execute on your machine at your next commit. With no `artifacts` globs configured the workdir is not downloaded at all, and `sync.note` says so. A download failure does not fail the run; the error lands in `sync.error` and the exec's exit code stands.

## 5. Bake the dependencies into the image

Every launch of a plain image pays dependency installation inside the guest, and it needs `--egress` to do it. `build --project` moves that work into the image:

```bash
microvm build --project . --reuse --name ci --json
```

Exactly one ecosystem's manifest and lockfile pair must be present in the directory: `pyproject.toml` with `uv.lock`, `package.json` with `package-lock.json`, or `Cargo.toml` with `Cargo.lock`. Only that pair is zipped into the build context beside the Dockerfile; nothing else in the directory enters the shared image snapshot. The derived Dockerfile copies the pair into `/project` and installs from the lockfile (`uv sync --locked`, `npm ci`, or `cargo fetch --locked`), each of which refuses a lockfile that disagrees with its manifest.

With `--reuse`, the pair joins the content hash. The image name becomes `<name>-<hash12>`, an unchanged project reuses its image in well under a second with `reused: true`, and a lockfile-only edit builds a fresh image under a new name. The lockfile is the identity, and the layer follows it: [Platform](/internals/platform/) measured a launch-to-first-import delta of about twenty seconds for the smallest project `--project` accepts, paid for by a build about fifteen seconds longer.

Take the image identifier off the build envelope and launch the project against it:

```bash
IMAGE=$(microvm build --project . --reuse --name ci --json \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['data']['imageIdentifier'])")
microvm run . --image "$IMAGE" --exec "make test"
```

The exec environment carries no `PATH` and no `HOME`, so a Python workload should call `/project/.venv/bin/python` directly or receive a `PATH` through `--launch-env`.

## 6. Keep syncing while you work

For a kept VM, `microvm sync . --name dev` syncs a project directory into the running VM's `/workspace`, uploading only what changed, with the same skip list and budgets as `run <DIR>`. `--watch` keeps syncing on filesystem changes until Ctrl-C, and `--full` uploads the whole tree even when the guest manifest claims members are unchanged.

:::agent

**For an agent.** Tell the exits apart before retrying anything. Exit 13 (`ERR_EXEC_FAILED`) is the workload's own failure, and the artifacts are already back. Exit 16 (`ERR_SYNC`) is a pack or extraction failure on this machine's filesystem, and the message names the subtree. Exit 12 (`ERR_PRECONDITION`) with a directory positional usually means nothing supplied an image. Read `data.sync.error` and `data.sync.note` before concluding artifacts are missing.

:::

Next: [drive it from code](/learn/tutorial/from-code/).
