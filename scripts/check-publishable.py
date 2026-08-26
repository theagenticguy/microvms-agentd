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
import subprocess
import sys

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


def main() -> int:
    packages = metadata()["packages"]
    failures: list[str] = []

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
