#!/usr/bin/env bash
# Prove the release actually shipped, as an exit code. Run by
# scripts/release-ship.sh once the tag's own workflow is green, and runnable by
# hand as `scripts/release-verify.sh <version>`.
#
# A workflow's exit code is not the registry's own state, so this reads the
# public record directly: the tag, the packages, the archives, the published
# body and a real install.
#
# Needs only git, curl, jq and gh for the checks that prove the release is
# public. npm is used only for the one check that installs it, and that check
# is skipped with a note when npm is missing. Nothing here authenticates to
# npm: every read is of a public package.
set -euo pipefail

say() { printf '%s\n' "$*" >&2; }
die() { say "release-verify: $*"; exit 1; }

registry=https://registry.npmjs.org

git fetch --quiet --tags --force origin

version="${1:-}"
[ -n "$version" ] || die "usage: release-verify.sh <version>"
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

# 2. That release commit was not taken back out again. `main` is never
#    force-pushed, so a release backed out after it landed is a revert: the
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
# npm answers "being processed and may take a few minutes" to a publish, and
# a check run the minute after the workflow finished found most packages
# missing that all appeared within three. So each one gets ten minutes.
npm_wait() {
  local name="$1" got="" tries=0
  while :; do
    got="$(curl -fsS "$registry/$name/$version" 2>/dev/null | jq -r '.version // empty')" || true
    [ "$got" = "$version" ] && return 0
    tries=$((tries + 1))
    [ "$tries" -lt 20 ] || die "npm: ${name/\%2f//} has no $version after 10 minutes (got '${got:-nothing}')"
    sleep 30
  done
}
for pkg in "${pkgs[@]}"; do
  # A scoped name is one path segment on the registry: the slash is %2f.
  npm_wait "$scope%2f$pkg"
done
npm_wait spoolway

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
# Read whole before awk sees it: awk stops at the section's end, and once the
# file outgrew the 64 KiB pipe buffer a piped `git show` died of SIGPIPE, which
# pipefail and set -e turned into a silent exit 141.
changelog="$(git show "$tag:CHANGELOG.md")" \
  || die "no CHANGELOG.md at $tag"
section="$(awk -v v="$version" '
  $0 ~ "^## " v "( |$)" { if (p) exit; p = 1 }
  p && /^## / && $0 !~ "^## " v "( |$)" { exit }
  p' <<<"$changelog")"
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
  # The install reads the package documents, which npm's CDN can serve stale
  # for a while after the per-version reads above already succeed, so it gets
  # a few tries of its own.
  for attempt in 1 2 3 4 5; do
    (
      cd "$install_dir"
      npm install --silent --prefer-online "spoolway@$version" >/dev/null
    ) && break
    [ "$attempt" -lt 5 ] || die "npm install spoolway@$version failed in $install_dir"
    say "release-verify: npm install spoolway@$version failed (attempt $attempt) — trying again in 30s"
    sleep 30
  done
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
