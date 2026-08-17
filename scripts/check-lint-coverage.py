#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
# SPDX-License-Identifier: Apache-2.0
"""Assert the ruff gate is looking at every Python file, rather than at none of them.

The defect this exists for is not a lint finding, it is a lint gate that passed over an
empty set. `mise run lint` ran `ruff check conformance scripts` for the whole life of that
line, and `scripts/` contributed nothing to it: ruff discovers Python by **extension** when
it walks a directory, every gate in `scripts/` was an extensionless PEP 723 script, so ruff
warned "No Python files found under the given path(s)" and exited 0. Green tick, zero files.
That is the shape `scripts/check-model-drift.py`'s own header names — "a green tick over three
silent checks is the failure mode" — and it hid real findings, since the first script to be
named explicitly turned out to have unused imports in it.

The fix is now the file names: the gates are `scripts/*.py`, so ruff finds them the ordinary
way. `ruff.toml`'s `extend-include = ["scripts/*"]` was the earlier fix and is gone, because
it taught only ruff — the identical blindness showed up next in
`scripts/check-license-headers.py`, which enumerated with `git ls-files "*.rs" "*.py"` at the
time (it covers six extensions now) and had therefore never seen these scripts either
(issue #32).

This is the guard *over* that, and it is separate for a reason: a naming convention nothing
checks is a convention, and a file that stops matching fails the same silent way the original
bug did. `ruff check .` would still print "All checks passed!" over nothing.

So this compares two independently derived answers to "what is the Python in this repo":

  **expected** — every tracked file that is Python, read off git. That is `*.py` and `*.pyi`
  by extension, plus every tracked file whose first line is a `python` or `uv run` shebang.
  The shebang half is what enforces the `.py` convention rather than trusting it: a new gate
  dropped in extensionless is Python to this enumerator and invisible to ruff, so it is
  reported by name. A new script under `scripts/` joins this set by existing, either way.

  **actual** — `ruff check --show-files`, which is ruff reporting the files it would
  inspect under its own configuration.

Any file in the first set and not the second is the bug, by name. The count is reported
either way, because a file count beside a passing gate is the line that would have made the
original defect obvious to a reader.

Two further assertions, each about a way the comparison itself could go quiet:

  1. `expected` must not be empty. Two derived sets agreeing on nothing is not agreement.
     `MIN_EXPECTED` is a floor on the *enumerator*, not on the repo — it fires when git
     answers nothing (wrong directory, a broken checkout), not when a file is deleted.
  2. The formatter is counted separately from the linter, because `ruff format` resolves its
     own `exclude` and can therefore go blind over a file the linter still reads. Its
     inspected count comes off the summary line it prints, and the deliberately unformatted
     files are read out of `ruff.toml` rather than repeated here. What this catches is
     format-side blindness the config does not account for; **widening** `[format] exclude`
     lowers both sides and passes, by design — that is a config edit visible in a diff, and
     `ruff.toml` is where each entry states its reason.

There used to be a third, and it went away with `extend-include`. That glob claimed every file
under `scripts/` was Python, so a shell helper dropped there was handed to ruff and reported as
a wall of syntax errors naming the symptom rather than the cause; this gate reported that case
directly instead. With the glob gone ruff skips a `.sh` by extension like anything else, so the
assertion had no hazard left to guard and its message would have stated a mechanism that no
longer exists. Removed rather than reworded, because a finding whose explanation is false is
worse than no finding.

One hole is left open knowingly: a file under `scripts/` that is Python, has no `.py`
extension, **and** has no shebang is invisible to both sides here, so it agrees vacuously. It
is also invisible to `uv`, to `python`, and to a reader, which is to say it is not a script
anybody can run — the smallest reachable version of this is a module a `.py` gate imports, and
that import would fail. If that ever stops being true, the fix is the same as the last two
times: name the file for what it is.

Usage:
    scripts/check-lint-coverage.py           # the guard. Offline, free, milliseconds
    scripts/check-lint-coverage.py --root D  # hand ruff a different path; the expected set
                                             # still comes from the repo. This is how the
                                             # gate is proven able to fail: point it at a
                                             # directory with no Python and it reports every
                                             # expected file as unseen, which is exactly the
                                             # original defect reproduced on purpose.
"""

from __future__ import annotations

import argparse
import os
import re
import shlex
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CONFIG = REPO / "ruff.toml"

#: A floor on the enumerator, not on the repo. See assertion 1 in the docstring: this
#: number exists so that `git ls-files` answering nothing is a failure rather than a
#: vacuous pass, and it is deliberately well below the real count so that deleting a file
#: does not require editing it. The name-by-name comparison is what catches a shortfall.
MIN_EXPECTED = 8

#: Matches the interpreter line on the PEP 723 gates (`#!/usr/bin/env -S uv run --script`)
#: and on a plain `#!/usr/bin/env python3`. Read from the file rather than inferred from the
#: directory, so a shell script in `scripts/` is classified as what it is.
#:
#: Every gate is `scripts/*.py` today, so nothing in the repo currently reaches `expected`
#: through this pattern rather than through its extension — the shebang pass finds no file the
#: extension pass had not already found. It stays because that is exactly the state it is
#: guarding: it is what fails the day someone adds an extensionless gate, which is the defect
#: this file and issue #32 were both about. Measured, with a `scripts/tmp-new-gate` carrying a
#: `uv run --script` shebang: reported by name as unlinted, exit 1.
PY_SHEBANG = re.compile(r"^#!.*(?:\bpython|\buv\s+run\b)")

#: `ruff format --check`'s summary: "N files would be reformatted, M files already
#: formatted", either half alone, and the singular "1 file". The two numbers sum to what
#: the formatter inspected, which is the only count it reports.
#:
#: A floor rather than an equality, because that sum is version-dependent and legitimately
#: larger than the Python surface: ruff 0.16 formats Python inside Markdown fences, so it
#: reports 66 files over this repo's 25 `docs/*.md` and 3 workflows where 0.15.22 reports 17.
#: More is not a defect. Fewer than the expected Python is.
FORMAT_TALLY = re.compile(r"(\d+) files? (?:would be reformatted|already formatted)")


def tracked() -> list[Path]:
    """Every tracked path, as `git ls-files` gives them: relative to the repo root."""
    out = subprocess.run(
        ["git", "ls-files"],
        capture_output=True,
        text=True,
        check=True,
        cwd=REPO,
    )
    return [Path(line) for line in out.stdout.splitlines() if line]


def first_line(path: Path) -> str:
    try:
        with (REPO / path).open("rb") as handle:
            return handle.readline().decode("utf-8", "replace").rstrip("\n")
    except OSError:
        # A tracked path that is not readable here (a submodule stub, a broken symlink) is
        # not something ruff would read either, so it is not a shortfall.
        return ""


def python_surface() -> set[Path]:
    """Every tracked file that is Python: by extension, or failing that by shebang.

    The shebang pass finds nothing the extension pass missed today, and that is the point of
    it — see `PY_SHEBANG`. It is what makes the `.py` naming an enforced property rather than
    a habit.
    """
    expected: set[Path] = set()
    for path in tracked():
        if path.suffix in {".py", ".pyi"} or PY_SHEBANG.match(first_line(path)):
            expected.add(path)
    return expected


def ruff_command() -> list[str]:
    """How to invoke ruff here, which is not the same answer in every environment.

    `ruff` is on PATH under mise, which is how `mise run lint` reaches it. CI installs
    nothing and calls `uvx ruff`, so a bare `ruff` there is a `FileNotFoundError` and this
    gate fails on its own tooling rather than on the coverage it exists to check. Measured:
    that is exactly how it first failed in CI. `RUFF` overrides both, so a caller pinning a
    version gets the version they pinned.
    """
    override = os.environ.get("RUFF")
    if override:
        return shlex.split(override)
    if shutil.which("ruff"):
        return ["ruff"]
    if shutil.which("uvx"):
        return ["uvx", "ruff"]
    raise SystemExit(
        "check-lint-coverage: no ruff found. Install it (mise install), or set RUFF to how "
        "to run it (for example RUFF='uvx ruff@0.15.22')."
    )


def inspected(root: str) -> set[Path]:
    """What `ruff check` says it would read under `root`, relative to the repo.

    `.toml` is dropped: ruff lists `pyproject.toml` and `ruff.toml` here because it lints
    its own settings tables, which is not the Python surface this gate is about.
    """
    out = subprocess.run(
        [*ruff_command(), "check", "--show-files", root],
        capture_output=True,
        text=True,
        check=True,
        cwd=REPO,
    )
    files: set[Path] = set()
    for line in out.stdout.splitlines():
        if not line:
            continue
        path = Path(line)
        if path.suffix == ".toml":
            continue
        files.add(path.relative_to(REPO) if path.is_absolute() else path)
    return files


def format_excluded() -> set[Path]:
    """`[format] exclude` from `ruff.toml`, read rather than restated.

    Repeating the list here would let the two drift, and a coverage gate that disagrees
    with the config about what is deliberately skipped reports the config as a defect.
    """
    if not CONFIG.is_file():
        return set()
    with CONFIG.open("rb") as handle:
        settings = tomllib.load(handle)
    return {Path(p) for p in settings.get("format", {}).get("exclude", [])}


def format_inspected(root: str) -> int:
    """How many files `ruff format --check` looked at, off its own summary line."""
    out = subprocess.run(
        [*ruff_command(), "format", "--check", root],
        capture_output=True,
        text=True,
        cwd=REPO,
    )
    # Exit code 1 here means "would reformat", which is a lint finding for `mise run lint`
    # to report and not this gate's subject. The counts are on stdout either way.
    return sum(int(n) for n in FORMAT_TALLY.findall(out.stdout))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        default=".",
        help="the path handed to ruff (default '.'); the expected set always comes"
        " from the repo, so a root with no Python fails loudly",
    )
    args = parser.parse_args()

    expected = python_surface()
    findings: list[str] = []

    if len(expected) < MIN_EXPECTED:
        findings.append(
            f"the enumerator found only {len(expected)} Python files in the repo,"
            f" below the floor of {MIN_EXPECTED}. `git ls-files` answering (almost)"
            " nothing makes every comparison below vacuous, so this is a failure"
            " rather than a clean run."
        )

    seen = inspected(args.root)
    unseen = sorted(expected - seen)
    if unseen:
        findings.append(
            f"ruff inspects {len(seen)} files under '{args.root}' and the repo has"
            f" {len(expected)}. {len(unseen)} Python files are NOT linted:\n"
            + "\n".join(f"    {path}" for path in unseen)
            + "\n  ruff finds Python by extension when it walks a directory, so a gate"
            " under scripts/ must be named `.py` to be seen. Rename it (keep the shebang"
            " and the executable bit; `./scripts/name.py` still runs). That is the defect"
            " this gate exists for: ruff exits 0 over files it never opened."
        )

    want_formatted = len(expected - format_excluded())
    got_formatted = format_inspected(args.root)
    if got_formatted < want_formatted:
        findings.append(
            f"ruff format inspects {got_formatted} files under '{args.root}', and"
            f" {want_formatted} are expected ({len(expected)} Python files less the"
            " ones `[format] exclude` names). The formatter resolves its own excludes,"
            " so it can go blind while the linter still reads the file."
        )

    if findings:
        print("lint coverage: FAILED", file=sys.stderr)
        for finding in findings:
            print(f"  {finding}", file=sys.stderr)
        return 1

    print(
        f"lint coverage: ruff lints all {len(expected)} Python files"
        f" ({len(expected) - want_formatted} excluded from the formatter by name)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
