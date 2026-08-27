#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Create and push a release tag, refusing every precondition a release should not proceed
# without.
#
# The refusals are not hypothetical. A leftover local `v0.1.0` from three weeks earlier
# survived on this machine; `git tag -a v0.1.0` failed with "tag already exists", the failure
# was not fatal to the surrounding command sequence, and the following `git push origin v0.1.0`
# pushed the OLD tag — pointing at a commit predating the crate rename, the publish flags, and
# the release workflow itself. Nothing ran only because `release.yml` does not exist at that
# commit, since GitHub resolves a workflow from the tagged ref. That is luck, not a control.
#
# No CI check can replace this one. An ancestry test passes a stale tag, because an old commit
# on main is a genuine ancestor of main, and the tag-versus-manifest comparison passes it too,
# because every version here read `0.1.0` for weeks. The only place the two are distinguishable
# is before the tag exists.
set -euo pipefail

TAG=${1:-}
if [[ -z $TAG ]]; then
  echo "usage: $0 vX.Y.Z" >&2
  exit 2
fi
if [[ ! $TAG =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "refusing: '$TAG' is not a vX.Y.Z tag; release.yml triggers on 'v*'" >&2
  exit 1
fi

fail() { echo "refusing: $*" >&2; exit 1; }

# A tag name is reusable in git and immutable on a registry. Reusing one silently releases
# whatever it already pointed at.
git rev-parse -q --verify "refs/tags/$TAG" >/dev/null \
  && fail "$TAG already exists locally (at $(git rev-parse --short "$TAG^{commit}")). Delete it with 'git tag -d $TAG' after confirming why it is there."
[[ -z $(git ls-remote --tags origin "refs/tags/$TAG") ]] \
  || fail "$TAG already exists on origin. A published version cannot be replaced."

[[ -z $(git status --porcelain) ]] || fail "the working tree is dirty; a release must describe a committed state."

git fetch --no-tags --quiet origin main
[[ $(git rev-parse HEAD) == "$(git rev-parse FETCH_HEAD)" ]] \
  || fail "HEAD is $(git rev-parse --short HEAD) and origin/main is $(git rev-parse --short FETCH_HEAD); release from main's tip."

# The same gate release.yml runs first, so a version disagreement is found before the tag
# exists rather than after.
./scripts/check-publishable.py "--tag=$TAG"

echo
echo "tagging $(git rev-parse --short HEAD) as $TAG"
git tag -a "$TAG" -m "$TAG"
git push origin "refs/tags/$TAG"
