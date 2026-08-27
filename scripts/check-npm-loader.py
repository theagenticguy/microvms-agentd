#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
# SPDX-License-Identifier: Apache-2.0
"""Assert the npm package ships a loader that can find the addons inside it.

`napi build` generates `microvms-js/index.js` and `index.d.ts`. Both are gitignored, and the
loader inside `index.js` is written against the package name that was in `package.json` at
generation time. Nothing regenerates it when that name changes and nothing compares the two.

Two failures, both measured on `@theagenticguy/microvms@0.1.0-rc.1` rather than imagined:

The loader can name a package that does not exist. That release shipped a loader generated while
the package was still the unscoped `microvms`, so it called `require('microvms-linux-x64-gnu')`.
`npm install` succeeded, npm resolved the right binding, and `require` failed with `Cannot find
native binding` — the defect sits entirely inside the tarball.

The loader can be absent altogether. The release workflow checks out the repo and downloads
`.node` artifacts without ever running `napi build`; with `index.js` gitignored, `files` named a
file that did not exist and `main` pointed at nothing.

Every platform's addon now ships in this one package, and the generated loader tries
`require('./<binaryName>.<suffix>.node')` BEFORE any per-platform package — which is what makes
one package viable and also what makes a missing binary fatal, since there is no longer anything
to fall back to. `--all-platforms` adds that check, and belongs wherever the full set has been
assembled. Without it only the loader itself is checked, which is what a job that built one
target can honestly assert.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

PACKAGE_JSON = Path("microvms-js/package.json")
LOADER = Path("microvms-js/index.js")
TYPES = Path("microvms-js/index.d.ts")

#: Rust triple -> the suffix napi derives from it, for both the package name and the `.node` file.
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
    all_platforms = "--all-platforms" in sys.argv
    manifest = json.loads(PACKAGE_JSON.read_text())
    name = manifest["name"]
    binary = manifest["napi"]["binaryName"]
    targets = manifest["napi"].get("targets") or []
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

    if "*.node" not in (manifest.get("files") or []):
        failures.append(
            f"{PACKAGE_JSON}'s `files` does not include `*.node`, so npm would publish the "
            f"loader without any of the binaries it loads."
        )

    for triple in targets:
        suffix = SUFFIX.get(triple)
        if suffix is None:
            failures.append(
                f"{triple} is configured and this gate does not know its napi suffix. Add it to "
                f"SUFFIX so the target is checked rather than silently skipped."
            )
            continue

        # Generated from `name`, so it is the cheapest witness that this loader was built for
        # THIS package rather than carried over from before a rename.
        expected = f"{name}-{suffix}"
        if expected not in loader:
            failures.append(
                f"{LOADER} never references {expected!r}, which is how it names the {triple} "
                f"binding. The loader was generated for a different package name, so `require` "
                f"fails on that platform even when npm installed everything correctly."
            )

        if not all_platforms:
            continue

        addon = LOADER.parent / f"{binary}.{suffix}.node"
        if not addon.is_file():
            failures.append(
                f"{addon} is absent, so {triple} has no binary in the published package. The "
                f"loader tries this path first and there is no per-platform package to fall "
                f"back to."
            )
        elif addon.stat().st_size == 0:
            failures.append(f"{addon} is empty")

    if failures:
        print("npm loader: FAIL", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    scope = f" and all {len(targets)} addons are present" if all_platforms else ""
    print(f"npm loader: names every configured platform under {name}{scope}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
