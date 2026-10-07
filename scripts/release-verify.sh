#!/usr/bin/env bash
# Prove the release the repository claims actually shipped, as an exit code.
# Run by the `released` step of the release pipeline, after `publish`.
#
# This step exists because a release is not `done` on anyone's say-so, agent
# or script — it is done when the tag, the packages, the archives and the
# published body actually exist. `publish` pushes the tag and watches its
# workflow, but a workflow exit code is not the registry's own state, and a
# release that recovered from a partial publish should not be trusted merely
# because the recovery lane reported success. This script reads the public
# record directly and is the real condition for `done`.
#
# Every check below reads the version from `origin/main`, so a `publish` that
# pushed nothing at all leaves this script proving the release that shipped
# last time and exiting 0 — which is how run `release-spoolway-3` reached
# `done` with no tag, no packages and nothing published. So the run is
# anchored to the candidate it was cut for: `.release-run/base-commit`,
# written by scripts/release-anchor.sh, is the commit `main` stood at when
# the worktree was cut. If `origin/main` is still sitting on it, no release
# commit was pushed and there is nothing here to verify.
#
# Needs only git, curl, jq and gh for the checks that prove the release is
# public. npm is used only for the one check that installs it — a release
# lane's own machine is not required to have node or npm, so that check is
# skipped with a note rather than failing the whole script when it is
# missing. Nothing here authenticates to npm: every read is of a public
# package, and the one install is of the just-published public wrapper.
set -euo pipefail

say() { printf '%s\n' "$*" >&2; }
die() { say "release-verify: $*"; exit 1; }

registry=https://registry.npmjs.org

git fetch --quiet --tags --force origin

# The frozen candidate, recorded by scripts/release-anchor.sh. Absent when the
# anchor step never ran — a checkout that was not cut for a release — and then
# there is no candidate to anchor to, so the unanchored behaviour is kept
# rather than failing a release over a missing hint. Present but not a commit
# is a different matter: something wrote it wrongly, and falling back would
# silently reopen the hole above.
anchor=""
anchor_file=.release-run/base-commit
if [ -e "$anchor_file" ]; then
  [ -r "$anchor_file" ] || die "$anchor_file is not readable"
  anchor="$(tr -d '[:space:]' < "$anchor_file")"
  case "$anchor" in
    "") die "$anchor_file is empty — scripts/release-anchor.sh did not finish; rerun it after deleting the file" ;;
    *[!0-9a-f]*) die "$anchor_file holds '$anchor', which is not a commit" ;;
  esac
fi

if [ -n "$anchor" ]; then
  head="$(git rev-parse origin/main)"
  # Prefix rather than equality: the file holds a full rev-parse when
  # scripts/release-anchor.sh writes it, but an abbreviated one written by hand
  # still names the same commit.
  if [ "${head#"$anchor"}" != "$head" ]; then
    die "origin/main is still at $head, the candidate this run was cut from — publish pushed no release commit, so there is nothing to verify"
  fi
  say "anchored to $anchor; origin/main has moved on to $head"
fi

version="$(git show origin/main:Cargo.toml | awk '/^\[package\]/{p=1;next} /^\[/{p=0} p && /^version *=/{gsub(/[" ]/,"",$3); print $3; exit}')"
[ -n "$version" ] || die "could not read the package version from origin/main:Cargo.toml"
tag="v$version"
say "verifying $tag"

# 1. The tag exists on the remote, and names a real release commit.
sha="$(git rev-list -n1 "$tag" 2>/dev/null)" || die "$tag does not exist locally after a --tags fetch"
git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null \
  || die "$tag is not on origin"
# The trailing ` (#123)` is not optional sloppiness: `main` is protected and
# takes no direct push, so a release commit can only arrive through a pull
# request, and squash merging appends the pull request number to the subject
# it lands. Requiring the bare subject made this script reject the only
# commit this repository is able to produce.
subject="$(git log -1 --format=%s "$sha")"
case "$subject" in
  "chore(release): $tag" | "chore(release): $tag (#"*")") ;;
  *) die "$tag names $sha, whose subject is '$subject', not 'chore(release): $tag'" ;;
esac
git merge-base --is-ancestor "$sha" origin/main \
  || die "the commit $tag names is not on origin/main"

# 2. That release commit was not taken back out again. This is the exact
#    wreckage the publisher leaves when a rehearsal goes red after the
#    release commit is pushed: it reverts rather than force-pushing, so the
#    bump is gone from the tree while the commit stays in the history.
# Matched with the same tolerance as the subject above, since GitHub names a
# revert after the subject it reverts, pull request number and all.
if git log --format=%s "$sha..origin/main" \
  | grep -qE "^Revert \"chore\(release\): ${tag//./\\.}( \(#[0-9]+\))?\"\$"; then
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

# 6. A real install, wrapper plus exactly one platform package, in a fixed-
#    purpose scratch directory that is removed whether the install succeeds
#    or not. Skipped with a note rather than failing the script when this
#    machine has no npm — see the header.
install_summary="npm is not on this machine, so the real-install check was skipped"
if command -v npm >/dev/null 2>&1; then
  install_dir="$(mktemp -d)"
  cleanup_install() { rm -rf "$install_dir"; }
  trap cleanup_install EXIT
  (
    cd "$install_dir"
    npm install --silent "spoolway@$version" >/dev/null
  ) || die "npm install spoolway@$version failed in $install_dir"
  got="$("$install_dir/node_modules/.bin/spoolway" --version)" \
    || die "the installed wrapper in $install_dir would not run"
  case "$got" in
    *"$version"*) ;;
    *) die "installed spoolway reports '$got', not $version" ;;
  esac
  say "release-verify: installed spoolway@$version in $install_dir and it reports $got"
  install_summary="a real install checked"
else
  say "release-verify: npm is not on this machine — skipping the real-install check"
fi

say "release-verify: $tag is published — tag on origin, ${#pkgs[@]} platform packages plus the wrapper, $expected release assets, a body matching the tagged section, and $install_summary"
