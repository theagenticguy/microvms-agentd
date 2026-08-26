#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
# SPDX-License-Identifier: Apache-2.0
"""Assert the workspace can still publish, and that only the intended crates would.

Two defect classes, and neither is visible from a green build.

The first is a manifest that cannot publish. `cargo publish` refuses a path dependency with
no `version`, requires `description` and `license`, requires the file named by `readme` to
exist, and caps keywords at five of twenty characters. None of that is checked by `cargo
build`, `cargo test`, or `cargo clippy` — so the manifest breaks at the moment a release tag
fires, which is the worst time to find out: the tag exists, the workflow has already started,
and half a workspace may already be on the registry. crates.io versions are immutable, so
there is no second attempt at `0.1.0`.

The second is a crate that publishes by accident. The workspace default is `publish = false`
and three crates opt in, so the failure shape is a *fourth* crate that quietly joins them —
`agentd-model` is a proof harness and `agentd` is guest software that reaches a consumer as a
binary baked into an image, and neither has anything a registry consumer could use. An
equality against a named set is what makes that impossible to do silently; a "does it look
publishable" heuristic would pass.

Offline by default, because `mise run check` is offline and free. `--dry-run` adds the tier
that needs the network: `cargo publish --workspace --dry-run`, which packages and verifies
every crate as if the others were already on the registry. That half runs in CI.

The `nothing to publish` check is not paranoia. With every crate unpublishable, `cargo publish
--workspace --dry-run` prints a warning and exits **0** — a gate wired to it alone would go
green forever the moment someone set `publish = false` at the root.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

#: The crates that go to crates.io, and nothing else.
#:
#: An allowlist, for the reason `microvms-cli/tests/thinness.rs` gives about its own
#: dependency set: a denylist is defeated by the one entry nobody thought to write down.
#: Adding a crate here is a decision someone makes on purpose, in a diff a reviewer sees.
PUBLISHED = {
    "microvms-protocol",
    "microvms-core",
    "microvms-cli",
}

#: Fields crates.io rejects an upload for, or renders an empty page without.
#:
#: `repository` is not required by the registry and is required here: it is what
#: `docs.rs`, `cargo vet`, and every provenance consumer resolve a crate back to its
#: source with, and a published crate that names no source is one nobody can audit.
REQUIRED = ("description", "license", "repository", "readme")

#: crates.io's own limits, which it enforces at upload and nothing enforces before.
MAX_KEYWORDS = 5
MAX_KEYWORD_LEN = 20
MAX_CATEGORIES = 5

#: Files whose `-p <crate>` selectors must name a crate that exists.
#:
#: Executable surfaces only. A workflow or a task that selects a missing package fails with
#: `package ID specification ... did not match any packages` and takes the build with it. A
#: markdown file showing the same string is prose, and `CLAUDE.md` deliberately quotes
#: `cargo test -p protocol` as the example of what *does not* work — so scanning docs here
#: would fail the gate on its own documentation.
SELECTOR_FILES = (
    ".github/workflows/ci.yml",
    ".github/workflows/docs.yml",
    "mise.toml",
)

#: `-p foo` and `--package foo`, the two spellings cargo accepts.
#:
#: Only applied to a line that invokes `cargo`, because `-p` is not cargo's alone: `mkdir -p
#: sbom` appears in both files above and matches this pattern exactly. That scoping is also
#: this check's limit — a `cargo` invocation whose selector sits on a shell continuation line
#: is not seen. Every one in this repo is on a single line, and a missed selector fails the
#: build the old way rather than passing something wrong.
SELECTOR = re.compile(r"(?:-p|--package)[ =]+([A-Za-z0-9_-]+)")


def metadata() -> dict:
    """The resolved workspace, read from cargo rather than by parsing TOML.

    `cargo metadata` applies workspace inheritance and resolves path dependencies, so a
    `publish` flag reached through `publish.workspace = true` is already collapsed to its
    real value here. Parsing the manifests by hand would check the files and not the build.
    """
    proc = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--offline"],
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        sys.exit(f"cargo metadata failed:\n{proc.stderr}")
    return json.loads(proc.stdout)


def publishable(package: dict) -> bool:
    """Whether cargo would let this package reach a registry.

    `publish` is `None` for "any registry" and a list of registry names otherwise, so the
    empty list is the `publish = false` case. Treating `None` as false would read every
    opted-in crate as private.
    """
    return package["publish"] != []


def stale_selectors(names: set[str]) -> list[str]:
    """Every `-p <crate>` in a workflow or task that names a package cargo cannot find.

    A package rename is the only thing that breaks these, which is why the check lives beside
    the publish set rather than in a gate of its own: a crate whose registry name differs from
    its directory name is exactly the situation that produces one. `protocol` became
    `microvms-protocol` because the bare name is taken on crates.io, and the two selectors in
    `ci.yml` that still said `-p protocol` failed only on macOS and Windows — the platforms
    whose tiers name crates explicitly, where ubuntu's `--all` passed and reported green.
    """
    failures: list[str] = []
    for relative in SELECTOR_FILES:
        path = Path(relative)
        if not path.exists():
            failures.append(f"{relative} is in SELECTOR_FILES and does not exist")
            continue
        for number, line in enumerate(path.read_text().splitlines(), start=1):
            # Comments describe intent and may name a crate that is gone on purpose.
            if line.lstrip().startswith("#") or "cargo" not in line:
                continue
            for selected in SELECTOR.findall(line):
                if selected not in names:
                    failures.append(
                        f"{relative}:{number} selects `-p {selected}`, which is not a package "
                        f"in this workspace. cargo fails with `package ID specification "
                        f"'{selected}' did not match any packages`."
                    )
    return failures


def tag_version_skew(tag: str, packages: list[dict]) -> list[str]:
    """Every manifest whose version disagrees with the release tag.

    The version a consumer receives is the one in the manifest, not the one in the tag, and
    they live in three unrelated files: the Cargo manifests, `microvms-py/pyproject.toml`, and
    `microvms-js/package.json`. A release that bumps two of the three publishes the wrong
    version to the third registry under a tag that claims otherwise, and every registry version
    is immutable — so the wrong number cannot be withdrawn, only superseded.

    Only the published crates are compared. `agentd` and `agentd-model` never reach a consumer,
    so their versions are free to diverge and a gate that forced them to match would be
    asserting tidiness rather than a contract.
    """
    expected = tag.removeprefix("v")
    failures: list[str] = []

    for package in sorted(packages, key=lambda p: p["name"]):
        if package["name"] in PUBLISHED and package["version"] != expected:
            failures.append(
                f"{package['name']} is version {package['version']} and the tag says "
                f"{expected} ({package['manifest_path']})"
            )

    for relative, found in (
        ("microvms-py/pyproject.toml", _toml_version("microvms-py/pyproject.toml")),
        ("microvms-js/package.json", _json_version("microvms-js/package.json")),
    ):
        if found != expected:
            failures.append(
                f"{relative} is version {found} and the tag says {expected}"
            )

    return failures


def _toml_version(relative: str) -> str | None:
    """`[project] version` without a TOML parser, because 3.11 is the floor and tomllib is 3.11+.

    Read with a regex rather than `tomllib` so this script keeps an empty dependency set: a
    release gate that needs a package installed is a gate that can fail for a reason unrelated
    to the release.
    """
    text = Path(relative).read_text()
    match = re.search(r'(?m)^version\s*=\s*"([^"]+)"', text)
    return match.group(1) if match else None


def _json_version(relative: str) -> str | None:
    return json.loads(Path(relative).read_text()).get("version")


def main() -> int:
    packages = metadata()["packages"]
    failures: list[str] = []

    failures.extend(stale_selectors({p["name"] for p in packages}))

    tag = next((a.split("=", 1)[1] for a in sys.argv if a.startswith("--tag=")), None)
    if tag:
        failures.extend(tag_version_skew(tag, packages))

    actual = {p["name"] for p in packages if publishable(p)}
    if actual != PUBLISHED:
        for name in sorted(actual - PUBLISHED):
            failures.append(
                f"{name} would publish and is not in PUBLISHED. Either add `publish = false` "
                f"to its manifest with the reason, or add it here on purpose."
            )
        for name in sorted(PUBLISHED - actual):
            failures.append(
                f"{name} is in PUBLISHED and would NOT publish. A release would skip it, and "
                f"anything depending on it would fail to resolve for consumers."
            )

    for package in sorted(packages, key=lambda p: p["name"]):
        if package["name"] not in PUBLISHED:
            continue
        where = package["manifest_path"]

        for field in REQUIRED:
            if not package.get(field):
                failures.append(f"{package['name']} declares no `{field}` ({where})")

        # The path is what cargo packages into the .crate; a name that resolves to nothing
        # fails the upload rather than shipping a crate with a blank registry page.
        readme = package.get("readme")
        if readme:
            expected = package["manifest_path"].rsplit("/", 1)[0] + "/" + readme
            try:
                open(expected).close()
            except OSError:
                failures.append(
                    f"{package['name']} names readme `{readme}`, which does not exist at "
                    f"{expected}"
                )

        keywords = package.get("keywords") or []
        if len(keywords) > MAX_KEYWORDS:
            failures.append(
                f"{package['name']} has {len(keywords)} keywords; crates.io accepts "
                f"{MAX_KEYWORDS}"
            )
        for keyword in keywords:
            if len(keyword) > MAX_KEYWORD_LEN:
                failures.append(
                    f"{package['name']} keyword {keyword!r} is {len(keyword)} characters; "
                    f"crates.io accepts {MAX_KEYWORD_LEN}"
                )
        if len(package.get("categories") or []) > MAX_CATEGORIES:
            failures.append(
                f"{package['name']} has more than {MAX_CATEGORIES} categories, which "
                f"crates.io rejects"
            )

        # A versionless path dependency is legal in a private workspace and rejected on
        # upload. cargo-deny's `wildcards = "deny"` covers the same ground from the other
        # side; this names the crate and the dependency instead of reporting a `*`.
        for dependency in package["dependencies"]:
            if dependency.get("path") and dependency["req"] == "*":
                failures.append(
                    f"{package['name']} depends on {dependency['name']} by path with no "
                    f'version. `cargo publish` rejects that: add `version = "..."` '
                    f"alongside `path`."
                )

    if failures:
        print("publishable: FAIL", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    print(
        f"publishable: {len(PUBLISHED)} crates, metadata complete "
        f"({', '.join(sorted(PUBLISHED))})"
    )
    if tag:
        print(f"publishable: every published manifest agrees with tag {tag}")

    if "--dry-run" in sys.argv:
        print("publishable: cargo publish --workspace --dry-run (needs the network)")
        proc = subprocess.run(
            ["cargo", "publish", "--workspace", "--dry-run", "--locked"],
            capture_output=True,
            text=True,
            check=False,
        )
        output = proc.stdout + proc.stderr
        print(output, end="" if output.endswith("\n") else "\n")
        if proc.returncode != 0:
            print("publishable: the dry run failed", file=sys.stderr)
            return 1
        if "nothing to publish" in output:
            print(
                "publishable: the dry run published nothing and still exited 0. Every crate "
                "is `publish = false`, so this gate was asserting over an empty set.",
                file=sys.stderr,
            )
            return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
