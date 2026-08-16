#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
# SPDX-License-Identifier: Apache-2.0
"""Regenerate `microvms-py/microvms.pyi` from the pyo3 surface itself.

`./scripts/generate-py-stubs.py` writes the file; `--check` compares without writing and
exits non-zero when the committed copy is stale. Same shape and same argument as
`agentd/src/bin/schema.rs`: a generated artifact consumers trust *because* it is
generated is the most dangerous thing to leave stale, and a reviewer cannot see the
staleness in a diff because the diff is the absence of a change.

What the drift is, concretely
----------------------------

`microvms-py` shipped no stub and no `py.typed` at all, which meant mypy/pyright/ty saw
every return value as `Any`. For an SDK whose thesis is that platform traps are closed in
the *type system*, that is the whole thesis made invisible to the one tool a Python caller
would use to check it: `float(usd)` raising `TypeError` at runtime is a closure nobody
finds until they run the code, and `EstimatedUsd.amount` answering a string is a decision a
checker could have enforced at edit time.

Three stages, and why each is here
----------------------------------

1. **maturin's own `generate-stubs`**, over a `--features stubs` build. maturin reads
   pyo3's `experimental-inspect` introspection data straight out of the compiled cdylib,
   so the signatures and the doc comments come from the Rust source rather than from a
   second hand-written description of it. This is deliberately NOT `pyo3-stub-gen`: that
   crate wants a `#[gen_stub_pyclass]` attribute on all 26 classes plus a `define_stub_info_gatherer!`
   and a bin target, which is a second surface declaration to keep in sync — the exact
   drift this script exists to prevent — and a new runtime dependency for the wheel.
   maturin is already the build backend, so the built-in path adds no dependency.

   Getting real output from it needed the `#[pymodule]` in `src/lib.rs` to move from the
   imperative `fn` form to the declarative `mod` form. pyo3 only records module *members*
   for the declarative form; with the `fn` form and its `add_class` calls, the
   introspection blob says `"incomplete":true,"members":[]` and maturin emits a six-line
   stub whose entire content is `def __getattr__(name: str) -> Incomplete: ...`. That
   file is worse than no file, because `py.typed` beside it promises a checker the
   package is typed and then answers every question with `Any`.

2. **Drop maturin's trailing `__getattr__` escape hatch.** maturin appends it because a
   module *could* have members it cannot see. Ours cannot: every class and function is a
   `#[pymodule_export]`, and the exceptions are enumerated in stage 3. Left in, it makes
   the stub unable to fail — measured here, `from microvms import CompletelyMadeUpName`
   type-checked clean with the line present and is an `unresolved-import` without it. A
   stub that cannot make a checker fail is decoration.

3. **Append the 14 exceptions and `__version__`.** These are the one part of the surface
   pyo3 does not introspect: `create_exception!` builds a type at runtime rather than
   through the `#[pyclass]` macro, so it emits no introspection record and maturin cannot
   see it. They are read out of the *imported module* instead — the built extension is
   imported and asked for its own exception classes, their base classes, and their
   docstrings — so this stage is still a reading of the compiled artifact rather than a
   list typed out by hand. A hierarchy added to `errors.rs` shows up here without anyone
   editing this script; a hierarchy that drifts fails `--check`.

   The four attributes every raised exception carries (`code`, `kind`, `wire_kind`,
   `retryable`) are declared on the base class. They are set with `setattr` in
   `to_py_err`, so nothing about the type could reveal them, and they are the entire
   reason the errors module says "nobody should parse a message" — a caller who cannot
   see them in a checker goes back to parsing messages.

Exit 0 when the committed stub matches, 1 on drift or on a build that failed.
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CRATE = REPO / "microvms-py"
ARTIFACT = CRATE / "microvms.pyi"

# Pinned rather than floating, and pinned to the same minor the CI `bindings` job
# installs. maturin's stub generation reads pyo3's *experimental* introspection format —
# pyo3's own word for it — so the writer and the format can disagree across releases. A
# floating `maturin@latest` here would turn someone else's release into this repo's drift
# failure, on a commit that changed nothing.
MATURIN = "maturin@1.14.1"


def run(
    argv: list[str], label: str | None = None, **kwargs: object
) -> subprocess.CompletedProcess[str]:
    """Runs a command, echoing it first so a CI log shows what produced the artifact.

    `label` replaces the echoed command for the one call whose argv is a multi-line Python
    program. Echoing that verbatim buries the result under twelve lines of source in every
    green run, and a log nobody reads to the end is where a real message goes to hide.
    """
    print(f"  $ {label or ' '.join(argv)}", file=sys.stderr)
    return subprocess.run(argv, text=True, check=True, **kwargs)  # type: ignore[arg-type]


def maturin_stub(out_dir: Path) -> str:
    """Stage 1: maturin's generation over a `--features stubs` build."""
    run(
        [
            "uvx",
            MATURIN,
            "generate-stubs",
            "-m",
            str(CRATE / "Cargo.toml"),
            "--features",
            "stubs",
            "--out",
            str(out_dir),
            "--quiet",
        ]
    )
    produced = out_dir / "microvms.pyi"
    if not produced.is_file():
        raise SystemExit(
            f"maturin wrote no stub to {produced}\nregenerate with: mise run stubs"
        )
    return produced.read_text()


def build_extension(out_dir: Path) -> Path:
    """Builds the cdylib and returns an importable `microvms.so` for stage 3.

    A plain `cargo build`, then a copy under the module's own name, rather than a wheel
    build and an install: importing the `.so` directly is what makes this readable without
    a venv to manage, and the exception hierarchy is defined by the module's *init*, which
    runs identically either way. `--features stubs` is not needed here — the introspection
    symbols are stage 1's business — but it is passed anyway so both stages read one build
    rather than forcing cargo to relink between them.
    """
    run(
        [
            "cargo",
            "build",
            "--quiet",
            "-p",
            "microvms-py",
            "--features",
            "stubs",
        ],
        cwd=REPO,
    )
    built = REPO / "target" / "debug" / "libmicrovms.so"
    if not built.is_file():
        raise SystemExit(f"cargo produced no cdylib at {built}")
    importable = out_dir / "microvms.so"
    shutil.copy2(built, importable)
    return importable


def exception_lines(extension: Path) -> list[str]:
    """Stage 3: the exception hierarchy, read out of the imported extension.

    Imported in a subprocess rather than in this interpreter. The extension links a
    tokio runtime and the AWS SDK, and this process has to survive to write a file and
    exit with a meaningful status; a segfault or an `atexit` hook in a 200 MB debug
    cdylib should not be able to take the generator down with it.
    """
    probe = (
        "import importlib.util, json, sys\n"
        f"spec = importlib.util.spec_from_file_location('microvms', {str(extension)!r})\n"
        "module = importlib.util.module_from_spec(spec)\n"
        "spec.loader.exec_module(module)\n"
        "found = []\n"
        "for name in dir(module):\n"
        "    value = getattr(module, name)\n"
        "    if isinstance(value, type) and issubclass(value, BaseException):\n"
        "        found.append({'name': name, 'base': value.__bases__[0].__name__,\n"
        "                      'doc': value.__doc__})\n"
        "json.dump({'exceptions': found, 'version': getattr(module, '__version__', None)},\n"
        "          sys.stdout)\n"
    )
    completed = run(
        [sys.executable, "-c", probe],
        label=f"{sys.executable} -c <read the exception hierarchy from {extension.name}>",
        capture_output=True,
    )
    payload = json.loads(completed.stdout)
    found = payload["exceptions"]
    if not found:
        raise SystemExit(
            "the built extension exposes no exception classes, which cannot be right:\n"
            "microvms-py/src/errors.rs defines a hierarchy under MicrovmError"
        )

    lines: list[str] = []
    version = payload["version"]
    if version is not None:
        lines += [
            "",
            "# `microvms_core::VERSION`, added by the module's `#[pymodule_init]`. The **core's**",
            "# version and not this crate's: what a caller needs to know is which client they are",
            "# talking through.",
            f'__version__: Final[str] = "{version}"',
        ]

    # Base class first, then the rest alphabetically. `__mro__` order would be the other
    # defensible choice and reads worse: a reader wants the base and its four attributes
    # before the thirteen one-line subclasses that inherit them.
    def sort_key(entry: dict[str, str]) -> tuple[int, str]:
        return (
            0 if entry["base"] not in {e["name"] for e in found} else 1,
            entry["name"],
        )

    for entry in sorted(found, key=sort_key):
        lines.append("")
        lines.append(f"class {entry['name']}({entry['base']}):")
        doc = (entry["doc"] or "").strip()
        if doc:
            lines.append('    """')
            for chunk in wrap_doc(doc):
                lines.append(f"    {chunk}" if chunk else "")
            lines.append('    """')
        if entry["base"] == "Exception":
            # The four attributes `to_py_err` attaches with `setattr`. Declared on the base
            # so every subclass inherits them, which is what lets a caller read `.code`
            # off any raised exception instead of parsing the message.
            lines += [
                "    code: str",
                "    kind: str",
                "    wire_kind: str | None",
                "    retryable: bool",
            ]
        elif not doc:
            lines.append("    ...")
    return lines


def wrap_doc(doc: str, width: int = 86) -> list[str]:
    """Wraps a docstring to the width the rest of the file uses, preserving blank lines."""
    out: list[str] = []
    for paragraph in doc.split("\n"):
        stripped = paragraph.strip()
        if not stripped:
            out.append("")
            continue
        line = ""
        for word in stripped.split():
            candidate = f"{line} {word}".strip()
            if len(candidate) > width and line:
                out.append(line)
                line = word
            else:
                line = candidate
        if line:
            out.append(line)
    return out


def render() -> str:
    """The whole artifact: maturin's stub, de-escaped, with the exceptions appended."""
    with tempfile.TemporaryDirectory() as tmp:
        out_dir = Path(tmp)
        generated = maturin_stub(out_dir)
        extension = build_extension(out_dir)
        exceptions = exception_lines(extension)

    lines = generated.rstrip("\n").split("\n")

    # Stage 2. Dropping the escape hatch is what makes an unknown name an error; see the
    # module docstring for the measurement.
    hatch = "def __getattr__(name: str) -> Incomplete: ..."
    if hatch in lines:
        lines.remove(hatch)
    else:
        # Not fatal, but worth saying: maturin stopped emitting the line, so the removal
        # is now a no-op and the next reader should check whether stage 2 is still needed.
        print(
            "note: maturin no longer emits the __getattr__ escape hatch;"
            " stage 2 of this script is now a no-op",
            file=sys.stderr,
        )

    # `Incomplete` was imported solely for that hatch. An unused import in a stub is not
    # merely untidy: ruff's own rules flag it, and this file is linted.
    if not any(
        "Incomplete" in line for line in lines if not line.startswith("from _typeshed")
    ):
        lines = [line for line in lines if line != "from _typeshed import Incomplete"]

    body = "\n".join(lines).rstrip("\n")
    banner = (
        "# SPDX-License-Identifier: Apache-2.0\n"
        "# GENERATED by ./scripts/generate-py-stubs.py — do not edit by hand.\n"
        "#\n"
        "# Regenerate with `mise run stubs`; `mise run stubs:check` fails when this file no\n"
        "# longer matches the Rust surface it describes. Signatures and docstrings above the\n"
        "# exceptions come from pyo3's introspection of `microvms-py/src/`; the exceptions\n"
        "# come from importing the built module, because `create_exception!` emits no\n"
        "# introspection record.\n"
    )
    return f"{banner}{body}\n" + "\n".join(exceptions).rstrip("\n") + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="compare without writing; exit 1 when the committed stub is stale",
    )
    args = parser.parse_args()

    expected = render()

    if not args.check:
        ARTIFACT.write_text(expected)
        print(f"wrote {ARTIFACT.relative_to(REPO)} ({len(expected)} bytes)")
        return 0

    if not ARTIFACT.is_file():
        print(
            f"{ARTIFACT.relative_to(REPO)} is missing.\n"
            "Regenerate it with: mise run stubs",
            file=sys.stderr,
        )
        return 1

    found = ARTIFACT.read_text()
    if found == expected:
        print(f"{ARTIFACT.relative_to(REPO)} is up to date")
        return 0

    print(
        f"{ARTIFACT.relative_to(REPO)} is stale: the pyo3 surface no longer matches the\n"
        "committed stub. A Python caller's type checker is now describing a surface this\n"
        "crate does not expose.\n"
        "Regenerate it with: mise run stubs\n"
        f"committed {len(found)} bytes, generated {len(expected)} bytes",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
