#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
# SPDX-License-Identifier: Apache-2.0
"""Assert every shipped Linux binary loads on the oldest host it claims to support.

Two artifacts, two contracts, one failure shape: a binary that needs a newer glibc than its
consumer has. It installs cleanly and fails at load, and it is invisible to every test that runs
on the image that built it.

For the npm addon the contract is `MAX_GLIBC` below. Measured on
`@theagenticguy/microvms-linux-x64-gnu@0.1.0-rc.1`, built natively on `ubuntu-latest` (Ubuntu
24.04): it required GLIBC_2.39, so `require` failed with ``/lib64/libc.so.6: version
`GLIBC_2.38' not found`` on a glibc 2.34 host. Building through `napi build --cross-compile`,
which routes via zig, brings both linux addons to GLIBC_2.30.

For the Python wheel the contract is the wheel's own `manylinux` tag, which is stricter and more
honest than a fixed ceiling: pip selects by that tag and trusts it, so a wheel claiming
`manylinux_2_17` while its extension needs more is an install that succeeds and an import that
does not. `maturin build --zig` produced a `manylinux_2_17_aarch64` wheel whose `.so` references
nothing above GLIBC_2.17, and that agreement is what `check_wheels` enforces rather than assumes.

Only ELF files are examined. The darwin and windows addons alongside them are not ELF and carry
no glibc requirement; they are reported as skipped rather than silently ignored, because a gate
that examines nothing and prints success is the failure this repository keeps finding — which is
also why an empty input is an error.

Versions are read by scanning for `GLIBC_<version>` byte strings rather than by parsing
`.gnu.version_r`, which keeps the dependency set empty and needs no binutils on the runner. That
can only over-report: a stray literal fails the gate loudly instead of letting a real one
through.
"""

from __future__ import annotations

import re
import sys
import zipfile
from pathlib import Path

#: The highest glibc an addon may require.
#:
#: 2.30 is what zig picks for a `*-gnu` target when no version is requested, measured on both
#: linux addons. It is not the floor this would prefer — `--use-napi-cross` produced 2.16, and
#: `cargo zigbuild --target aarch64-unknown-linux-gnu.2.17` produced exactly 2.17 — but neither
#: is reachable for the addon. The napi cross toolchain is glibc-2.17-era GCC and cannot compile
#: `aws-lc-sys` for aarch64 (no `stdatomic.h`, no `AT_HWCAP2`, no `-march=armv8.4-a+sha3`), and
#: `napi build` rejects a version-suffixed triple because it runs `cargo metadata` with it and
#: rustc does not recognise the target.
#:
#: So this ceiling buys a working aarch64 addon, and the cost is bounded: 2.30 excludes Amazon
#: Linux 2, RHEL 8 and Debian 10, all of which are EOL or in extended support. Every current
#: distribution loads it.
#:
#: The Python wheel is NOT held to this and keeps a 2.17 floor, because maturin passes the
#: manylinux glibc version to zig itself. The two artifacts having different floors is honest
#: rather than tidy, which is why each failure below names its own exclusion list instead of one
#: blanket claim.
MAX_GLIBC = (2, 30)

ELF_MAGIC = b"\x7fELF"
GLIBC_SYMBOL = re.compile(rb"GLIBC_(\d+)\.(\d+)(?:\.(\d+))?")


def required_glibc(data: bytes) -> tuple[int, ...] | None:
    """The highest `GLIBC_x.y` version referenced, or None if the file names none."""
    versions = {
        tuple(int(part) for part in match.groups() if part is not None)
        for match in GLIBC_SYMBOL.finditer(data)
    }
    return max(versions) if versions else None


#: `manylinux_<major>_<minor>_<arch>` inside a wheel filename, which is a wheel's own claim
#: about the oldest glibc it loads on.
MANYLINUX_TAG = re.compile(r"manylinux_(\d+)_(\d+)_")

#: `manylinux1`/`2010`/`2014` legacy aliases, mapped to the glibc they mean.
LEGACY_MANYLINUX = {
    "manylinux1": (2, 5),
    "manylinux2010": (2, 12),
    "manylinux2014": (2, 17),
}


def wheel_ceiling(name: str) -> tuple[int, ...] | None:
    """The glibc a wheel's own filename promises, or None if it makes no linux claim."""
    claims = [tuple(int(g) for g in m.groups()) for m in MANYLINUX_TAG.finditer(name)]
    claims += [v for alias, v in LEGACY_MANYLINUX.items() if alias in name]
    # The LOWEST claim binds: a wheel tagged both `manylinux_2_17` and `manylinux2014` promises
    # to load wherever either does, so the stricter promise is the one to hold it to.
    return min(claims) if claims else None


def check_wheels(roots: list[Path]) -> tuple[list[str], int]:
    """Assert each wheel's extension module honours the manylinux tag the wheel advertises.

    A wheel is selected by its tag, and pip trusts the tag. One that claims `manylinux_2_17`
    while its `.so` references GLIBC_2.28 installs cleanly and fails at import — the same shape
    as an addon with the wrong floor, but discovered by the consumer rather than by a build.

    Checked against the wheel's own claim rather than `MAX_GLIBC`, because a wheel states its
    contract in its filename and that is a stricter and more honest thing to enforce.
    """
    failures: list[str] = []
    checked = 0
    for wheel in sorted({p for root in roots for p in root.rglob("*.whl")}):
        ceiling = wheel_ceiling(wheel.name)
        if ceiling is None:
            print(f"  skipped (no manylinux claim): {wheel.name}")
            continue
        with zipfile.ZipFile(wheel) as z:
            members = [n for n in z.namelist() if n.endswith(".so")]
            if not members:
                failures.append(f"{wheel.name} claims manylinux and contains no `.so`")
                continue
            for member in members:
                data = z.read(member)
                if not data.startswith(ELF_MAGIC):
                    continue
                checked += 1
                needed = required_glibc(data)
                if needed is None:
                    continue
                shown = ".".join(str(p) for p in needed)
                promised = ".".join(str(p) for p in ceiling)
                if needed > ceiling:
                    failures.append(
                        f"{wheel.name} is tagged manylinux {promised} and its {member} requires "
                        f"GLIBC_{shown}. pip trusts the tag, so this installs and then fails to "
                        f"import."
                    )
                else:
                    print(
                        f"  {wheel.name}: tag {promised} holds ({member} needs {shown})"
                    )
    return failures, checked


def main() -> int:
    roots = [Path(arg) for arg in sys.argv[1:]] or [Path("microvms-js")]
    addons = sorted({p for root in roots for p in root.rglob("*.node")})
    wheel_failures, wheels_checked = check_wheels(roots)
    if wheels_checked and not addons:
        if wheel_failures:
            print("wheel glibc: FAIL", file=sys.stderr)
            for failure in wheel_failures:
                print(f"  - {failure}", file=sys.stderr)
            return 1
        print(
            f"wheel glibc: {wheels_checked} extension module(s) honour their manylinux tag"
        )
        return 0

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
            # States the measurement and the consequence, and NOT a cause. An earlier version of
            # this message asserted the addon "was built against the host's glibc", which was
            # wrong the first time it fired on a real artifact: a zig cross-build on a glibc 2.34
            # host produced a 2.30 floor, so the number came from neither the host nor the
            # toolchain the message named. A gate that guesses why is a gate that misdirects.
            excluded = ", ".join(
                f"{distro} ({v})"
                for distro, v in (
                    ("Amazon Linux 2 / RHEL 8", "2.28"),
                    ("Ubuntu 20.04", "2.31"),
                    ("Amazon Linux 2023 / RHEL 9", "2.34"),
                    ("Ubuntu 22.04", "2.35"),
                    ("Debian 12", "2.36"),
                )
                if tuple(int(x) for x in v.split(".")) < needed
            )
            failures.append(
                f"{addon.name} requires GLIBC_{shown}, above the {ceiling} ceiling. "
                f"Cannot load on: {excluded or 'none of the reference distributions'}."
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

    ceiling = ".".join(str(part) for part in MAX_GLIBC)
    print(f"addon glibc: {checked} Linux addon(s) within the GLIBC_{ceiling} ceiling")
    return 0


if __name__ == "__main__":
    sys.exit(main())
