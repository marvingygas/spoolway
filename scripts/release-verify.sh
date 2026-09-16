#!/usr/bin/env bash
# Prove the release the repository claims actually shipped, as an exit code.
# Run by the `released` step of the release pipeline, after `publish`.
#
# This step exists because `publish` is an agent step: under
# `unattended.skip_blocked_lane` a cleared block on an agent step carries the
# task one step past it, so a release could reach `done` without anything
# being published. A command step is never carried past, so whatever this
# script asserts is the real condition for `done`.
#
# Needs only git, curl, jq and gh — deliberately not node or npm, which a
# release lane's own machine is not required to have. Nothing here
# authenticates to npm: every read is of a public package.
set -euo pipefail

say() { printf '%s\n' "$*" >&2; }
die() { say "release-verify: $*"; exit 1; }

registry=https://registry.npmjs.org

git fetch --quiet --tags --force origin

version="$(git show origin/main:Cargo.toml | awk '/^\[package\]/{p=1;next} /^\[/{p=0} p && /^version *=/{gsub(/[" ]/,"",$3); print $3; exit}')"
[ -n "$version" ] || die "could not read the package version from origin/main:Cargo.toml"
tag="v$version"
say "verifying $tag"

# 1. The tag exists on the remote, and names a real release commit.
sha="$(git rev-list -n1 "$tag" 2>/dev/null)" || die "$tag does not exist locally after a --tags fetch"
git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null \
  || die "$tag is not on origin"
subject="$(git log -1 --format=%s "$sha")"
[ "$subject" = "chore(release): $tag" ] \
  || die "$tag names $sha, whose subject is '$subject', not 'chore(release): $tag'"
git merge-base --is-ancestor "$sha" origin/main \
  || die "the commit $tag names is not on origin/main"

# 2. That release commit was not taken back out again. This is the exact
#    wreckage the publisher leaves when a rehearsal goes red after the
#    release commit is pushed: it reverts rather than force-pushing, so the
#    bump is gone from the tree while the commit stays in the history.
if git log --format=%s "$sha..origin/main" | grep -qxF "Revert \"chore(release): $tag\""; then
  die "$tag was reverted on origin/main — the bump is not in effect"
fi

# 3. Every package the workflow publishes reports this version. Read the
#    platform list from the tagged tree, so it is the matrix that was
#    actually released rather than whatever the checkout carries now.
scope="$(git show "$tag:npm/targets.json" | jq -r .scope)"
mapfile -t pkgs < <(git show "$tag:npm/targets.json" | jq -r '.targets[].pkg')
[ "${#pkgs[@]}" -gt 0 ] || die "no platform packages listed in npm/targets.json at $tag"
for pkg in "${pkgs[@]}"; do
  # A scoped name is one path segment on the registry: the slash is %2f.
  got="$(curl -fsS "$registry/$scope%2f$pkg/$version" 2>/dev/null | jq -r '.version // empty')" || true
  [ "$got" = "$version" ] || die "npm: $scope/$pkg has no $version (got '${got:-nothing}')"
done
got="$(curl -fsS "$registry/spoolway/$version" 2>/dev/null | jq -r '.version // empty')" || true
[ "$got" = "$version" ] || die "npm: spoolway has no $version (got '${got:-nothing}')"

# 4. The GitHub release carries one archive per platform, plus the checksums.
assets="$(gh release view "$tag" --json assets --jq '.assets[].name')" \
  || die "no GitHub release for $tag"
count="$(printf '%s\n' "$assets" | grep -c . || true)"
expected=$(( ${#pkgs[@]} + 1 ))
[ "$count" -eq "$expected" ] \
  || die "$tag has $count release assets, expected $expected (one per platform plus SHA256SUMS)"
printf '%s\n' "$assets" | grep -qxF SHA256SUMS || die "$tag has no SHA256SUMS asset"

# 5. The published body is the tagged changelog section, byte for byte. The
#    workflow creates it with --notes-file from the tagged tree, so any
#    difference means it read a different tree. GitHub may store the body
#    with CRLF; that is the one difference that is not a failure.
# The heading is `## <version>` today; tags older than 177b963 carry a theme
# after it. Enforcing which of the two is legal belongs to the changelog
# contract in src/release_notes.rs — all this needs is the section's bounds.
section="$(git show "$tag:CHANGELOG.md" | awk -v v="$version" '
  $0 ~ "^## " v "( |$)" { if (p) exit; p = 1 }
  p && /^## / && $0 !~ "^## " v "( |$)" { exit }
  p')"
[ -n "$section" ] || die "no '## $version' section in CHANGELOG.md at $tag"
body="$(gh release view "$tag" --json body --jq .body | tr -d '\r')"
diff <(printf '%s\n' "$section" | sed -e 's/[[:space:]]*$//') \
     <(printf '%s\n' "$body" | sed -e 's/[[:space:]]*$//') \
  || die "the published release body is not the changelog section tagged at $tag"

say "release-verify: $tag is published — tag on origin, ${#pkgs[@]} platform packages plus the wrapper, $expected release assets, and a body matching the tagged section"
