#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
# SPDX-License-Identifier: Apache-2.0
"""Assert a shipped Linux addon does not demand a newer glibc than its oldest supported host.

A native `napi build` on a GitHub runner links against the runner's glibc, and the runner is
whatever `ubuntu-latest` currently points at. That version is a floor imposed on every consumer,
it is invisible in every test that runs on the same image that built the artifact, and it is
immutable once published.

Measured on `@theagenticguy/microvms-linux-x64-gnu@0.1.0-rc.1`, built natively on
`ubuntu-latest` (Ubuntu 24.04, glibc 2.39): the addon required `GLIBC_2.39`, so `require` failed
with ``/lib64/libc.so.6: version `GLIBC_2.38' not found`` on a glibc 2.34 host — Amazon Linux
2023, RHEL 9, Debian 12 and Ubuntu 22.04 are all below that line. The same build through
`napi build --use-napi-cross`, which downloads a toolchain pinned to glibc 2.17, requires no more
than `GLIBC_2.16` and loads on the same host.

So the gate is the flag's witness: it fails if a build stops going through that toolchain, which
is otherwise a one-word change with no local symptom.

Only ELF files are examined. The darwin and windows addons in the same directory are not ELF and
carry no glibc requirement; they are reported as skipped rather than silently ignored, because a
gate that examines nothing and prints success is the failure this repository keeps finding.

The versions are read by scanning for `GLIBC_<version>` byte strings rather than by parsing
`.gnu.version_r`, which keeps this script's dependency set empty and needs no binutils on the
runner. That can only over-report — a stray literal fails the gate loudly instead of letting a
real one through.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

#: The highest glibc an addon may require.
#:
#: `@napi-rs/cross-toolchain`, which `--use-napi-cross` downloads, is pinned to glibc 2.17, and a
#: build through it was measured at 2.16. The ceiling sits at the toolchain's own version rather
#: than at the measurement, so a dependency that legitimately needs 2.17 does not fail.
#:
#: Raising this is a decision about which distributions stop being able to install the package.
#: For reference: RHEL 9 and Amazon Linux 2023 are 2.34, Debian 12 is 2.36, Ubuntu 22.04 is 2.35.
MAX_GLIBC = (2, 17)

ELF_MAGIC = b"\x7fELF"
GLIBC_SYMBOL = re.compile(rb"GLIBC_(\d+)\.(\d+)(?:\.(\d+))?")


def required_glibc(data: bytes) -> tuple[int, ...] | None:
    """The highest `GLIBC_x.y` version referenced, or None if the file names none."""
    versions = {
        tuple(int(part) for part in match.groups() if part is not None)
        for match in GLIBC_SYMBOL.finditer(data)
    }
    return max(versions) if versions else None


def main() -> int:
    roots = [Path(arg) for arg in sys.argv[1:]] or [Path("microvms-js")]
    addons = sorted({p for root in roots for p in root.rglob("*.node")})

    if not addons:
        print(
            f"addon glibc: found no `.node` under {', '.join(str(r) for r in roots)}. This gate "
            f"asserts over built artifacts, so an empty set means it checked nothing.",
            file=sys.stderr,
        )
        return 1

    failures: list[str] = []
    checked = 0

    for addon in addons:
        data = addon.read_bytes()
        if not data.startswith(ELF_MAGIC):
            print(f"  skipped (not ELF, so no glibc floor): {addon.name}")
            continue
        checked += 1
        needed = required_glibc(data)
        if needed is None:
            print(f"  {addon.name}: references no glibc version")
            continue
        shown = ".".join(str(part) for part in needed)
        if needed > MAX_GLIBC:
            ceiling = ".".join(str(part) for part in MAX_GLIBC)
            failures.append(
                f"{addon.name} requires GLIBC_{shown}, above the {ceiling} ceiling. It was "
                f"built against the host's glibc instead of through `--use-napi-cross`, so it "
                f"will not load on RHEL 9, Amazon Linux 2023, Debian 12 or Ubuntu 22.04."
            )
        else:
            print(f"  {addon.name}: requires at most GLIBC_{shown}")

    if failures:
        print("addon glibc: FAIL", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    if checked == 0:
        print(
            "addon glibc: every addon found was non-ELF, so no Linux artifact was verified.",
            file=sys.stderr,
        )
        return 1

    print(f"addon glibc: {checked} Linux addon(s) within the GLIBC_2.17 ceiling")
    return 0


if __name__ == "__main__":
    sys.exit(main())
