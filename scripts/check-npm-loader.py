#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
# SPDX-License-Identifier: Apache-2.0
"""Assert the generated npm loader can find the platform packages it will be published beside.

`napi build` generates `microvms-js/index.js` and `index.d.ts`, both gitignored, and the
loader inside `index.js` is written against the package name that was in `package.json` at
generation time. Nothing regenerates it when that name changes and nothing compares the two, so
the root package can ship a loader that resolves a set of package names nobody publishes.

Measured, not hypothetical. `@theagenticguy/microvms@0.1.0-rc.1` shipped with a loader
generated while the package was still the unscoped `microvms`: it called
`require('microvms-linux-x64-gnu')` and `require('microvms-wasm32-wasi')`, so a consumer with
`@theagenticguy/microvms-linux-x64-gnu` correctly installed got `Error: Cannot find native
binding`. `npm install` succeeded, npm selected the right platform package, and `require`
failed — the failure is entirely inside the root tarball.

The absence case is worse and was latent in the release workflow. Its npm job checks out the
repo and downloads `.node` artifacts, never running `napi build`; with `index.js` gitignored,
`files` would name a file that does not exist and the root package would ship with `main`
pointing at nothing.

Both cases are the same assertion: for every triple in `napi.targets`, the loader must name
`<package-name>-<suffix>`. That ties the loader to the manifest rather than to whenever someone
last built.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

PACKAGE_JSON = Path("microvms-js/package.json")
LOADER = Path("microvms-js/index.js")
TYPES = Path("microvms-js/index.d.ts")

#: Rust triple -> the package-name suffix and `.node` infix napi derives from it.
#:
#: napi's own mapping, restated because the CLI exposes it only by doing the work. A triple this
#: does not know fails loudly rather than being skipped, since a skipped target is exactly the
#: defect being guarded against.
SUFFIX = {
    "x86_64-unknown-linux-gnu": "linux-x64-gnu",
    "aarch64-unknown-linux-gnu": "linux-arm64-gnu",
    "x86_64-unknown-linux-musl": "linux-x64-musl",
    "aarch64-unknown-linux-musl": "linux-arm64-musl",
    "aarch64-apple-darwin": "darwin-arm64",
    "x86_64-apple-darwin": "darwin-x64",
    "x86_64-pc-windows-msvc": "win32-x64-msvc",
    "aarch64-pc-windows-msvc": "win32-arm64-msvc",
}


def main() -> int:
    manifest = json.loads(PACKAGE_JSON.read_text())
    name = manifest["name"]
    targets = manifest.get("napi", {}).get("targets") or []
    failures: list[str] = []

    for path in (LOADER, TYPES):
        if not path.is_file():
            failures.append(
                f"{path} does not exist. `napi build` generates it and it is gitignored, so a "
                f"job that publishes without building must obtain it as an artifact — "
                f"`package.json`'s `files` names it and `main` points at it."
            )
    if failures:
        print("npm loader: FAIL", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    loader = LOADER.read_text()

    if not targets:
        failures.append(f"{PACKAGE_JSON} declares no `napi.targets`")

    for triple in targets:
        suffix = SUFFIX.get(triple)
        if suffix is None:
            failures.append(
                f"{triple} is configured and this gate does not know its napi suffix. Add it to "
                f"SUFFIX so the target is checked rather than silently skipped."
            )
            continue
        expected = f"{name}-{suffix}"
        if expected not in loader:
            failures.append(
                f"{LOADER} never references {expected!r}, which is the package that carries the "
                f"{triple} binary. The loader was generated for a different package name, so "
                f"`require` fails on that platform even when npm installed it correctly."
            )

    if failures:
        print("npm loader: FAIL", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    print(f"npm loader: resolves all {len(targets)} platform packages under {name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
