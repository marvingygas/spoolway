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
# Every check below reads the version from `origin/main`, so a `publish` that
# pushed nothing at all leaves this script proving the release that shipped
# last time and exiting 0 — which is how run `release-spoolway-3` reached
# `done` with no tag, no packages and nothing published. So the run is
# anchored to the candidate it was cut for: `SPOOLWAY_TASK_FILE`, the one
# variable a command step is given that leads back to this task, names the
# document whose `base_commit:` is the commit `main` stood at when the lane
# was cut. If `origin/main` is still sitting on it, no release commit was
# pushed and there is nothing here to verify. (Agent lanes are handed
# `SPOOLWAY_HEAD`, which carries the same fact in one word; command steps
# are not, so this reads the document instead.)
#
# Needs only git, curl, jq and gh — deliberately not node or npm, which a
# release lane's own machine is not required to have. Nothing here
# authenticates to npm: every read is of a public package.
set -euo pipefail

say() { printf '%s\n' "$*" >&2; }
die() { say "release-verify: $*"; exit 1; }

registry=https://registry.npmjs.org

git fetch --quiet --tags --force origin

# The frozen candidate, read out of the leading `---` block of the task
# document so a `base_commit:` written in the prose below it cannot be
# mistaken for the key. Empty when the variable is unset — run by hand, or by
# anything that is not a dispatched command step — and empty too when the
# document carries no such key, which is what a borrowed checkout records:
# neither of those has a candidate to anchor to, and both keep the
# unanchored behaviour rather than failing a release over a missing hint.
anchor=""
if [ -n "${SPOOLWAY_TASK_FILE:-}" ]; then
  # Set but unreadable is not the same absence: the dispatcher writes this
  # path itself, so a path that does not resolve means something is wrong
  # with the run, and falling back would silently reopen the hole above.
  [ -r "$SPOOLWAY_TASK_FILE" ] || die "SPOOLWAY_TASK_FILE names $SPOOLWAY_TASK_FILE, which is not readable"
  anchor="$(awk '
    NR == 1 { if ($0 != "---") exit; next }
    $0 == "---" { exit }
    /^base_commit:/ {
      sub(/^base_commit:[ \t]*/, "")
      gsub(/["\047]/, "")
      sub(/[ \t]*$/, "")
      print
      exit
    }
  ' "$SPOOLWAY_TASK_FILE")"
  case "$anchor" in
    "") ;;
    *[!0-9a-f]*) die "base_commit in $SPOOLWAY_TASK_FILE is '$anchor', which is not a commit" ;;
  esac
fi

if [ -n "$anchor" ]; then
  head="$(git rev-parse origin/main)"
  # Prefix rather than equality: `base_commit:` is a full rev-parse when the
  # dispatcher writes it, but an abbreviated one hand-edited into the
  # document still names the same commit.
  if [ "${head#"$anchor"}" != "$head" ]; then
    die "origin/main is still at $head, the candidate this task was cut from — publish pushed no release commit, so there is nothing to verify"
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

say "release-verify: $tag is published — tag on origin, ${#pkgs[@]} platform packages plus the wrapper, $expected release assets, and a body matching the tagged section"
