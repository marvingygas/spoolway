#!/usr/bin/env bash
# Prove the release actually shipped, as an exit code. Run by
# scripts/release-ship.sh once the tag's own workflow is green, and runnable by
# hand as `scripts/release-verify.sh <version>`.
#
# A workflow's exit code is not the registry's own state, so this reads the
# public record directly: the tag, the packages, the download files, the published
# body and a real install.
#
# Needs only git, curl, jq and gh for the checks that prove the release is
# public. npm is used only for the check that installs it from the registry,
# and brew only for the check that installs it from the tap. Each is skipped
# with a note when its tool is missing. Nothing here authenticates to npm or
# to the tap: every read is of a public package or repository.
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

# 4. The GitHub release carries one bare file per target that names a
#    `download`, plus the checksums, plus `install.sh` when the tagged tree has
#    scripts/install.sh. Releases tagged before targets.json gained that key
#    shipped one archive per platform instead, so a tagged tree where no
#    target has the key is counted by the old rule.
assets="$(gh release view "$tag" --json assets --jq '.assets[].name')" \
  || die "no GitHub release for $tag"
count="$(printf '%s\n' "$assets" | grep -c . || true)"
downloads="$(git show "$tag:npm/targets.json" \
  | jq -r 'if any(.targets[]; has("download")) then [.targets[] | select(.download != null)] | length else (.targets | length) end')"
expected=$(( downloads + 1 ))
has_installer=0
if git cat-file -e "$tag:scripts/install.sh" 2>/dev/null; then
  has_installer=1
  expected=$(( expected + 1 ))
fi
[ "$count" -eq "$expected" ] \
  || die "$tag has $count release assets, expected $expected (one per download file, or per platform archive on older tags, plus SHA256SUMS, plus install.sh when the tag has scripts/install.sh)"
printf '%s\n' "$assets" | grep -qxF SHA256SUMS || die "$tag has no SHA256SUMS asset"
if [ "$has_installer" = 1 ]; then
  printf '%s\n' "$assets" | grep -qxF install.sh || die "$tag has no install.sh asset"
fi

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

# 7. A real Homebrew install of the formula the release workflow pushed to the
#    tap, run wherever brew is present (it supports Linux, so the maintainer's
#    WSL qualifies). Skipped with a note, like the npm check, when brew is
#    missing. The brew copy is `$(brew --prefix)/bin/spoolway`, and brew's bin
#    can sit ahead of the npm copy on PATH, so everything this check added is
#    undone afterwards: the formula, the tap, and the trust entry that
#    Homebrew 6 writes to its trust.json when it installs from a tap. A copy
#    that was there before is upgraded and left installed, since removing it
#    would take something away from the maintainer.
#    The tap holds only the newest formula, so this check passes only for the
#    latest release. Run by hand for an older version it dies at the version
#    comparison, naming the version the tap holds, rather than skipping:
#    a skipped check would read as a pass.
brew_formula=marvingygas/tap/spoolway
brew_tap=marvingygas/tap
brew_summary="brew is not on this machine, so the brew install check was skipped"
# Whether the formula is trusted, from `brew trust --json=v1`. Homebrew before
# 6 has no such command, and then nothing was ever trusted. An entry counts
# if its text names the formula or the tap, whichever shape the entry has.
brew_trusted() {
  brew trust --json=v1 2>/dev/null \
    | jq -e --arg f "$brew_formula" --arg t "$brew_tap" \
        '[.formulae[]?, .taps[]? | tostring] | any(contains($f) or contains($t))' >/dev/null 2>&1
}
# Read whole before grep sees them: `grep -q` quits at the first match, and a
# listing killed by SIGPIPE under pipefail would read as "not there", which
# would make the cleanup uninstall a copy the maintainer already had.
brew_formula_present() { local l; l="$(brew list --full-name --formula)" && grep -qxF "$brew_formula" <<<"$l"; }
brew_tap_present() { local l; l="$(brew tap)" && grep -qxF "$brew_tap" <<<"$l"; }
if command -v brew >/dev/null 2>&1; then
  brew_formula_was=0
  brew_tap_was=0
  brew_trusted_was=0
  brew list --full-name --formula >/dev/null || die "brew list failed, so it is not known whether $brew_formula is installed"
  brew tap >/dev/null || die "brew tap failed, so it is not known whether $brew_tap is added"
  ! brew_formula_present || brew_formula_was=1
  ! brew_tap_present || brew_tap_was=1
  ! brew_trusted || brew_trusted_was=1
  # Runs on every exit, including a `die` halfway through the install, which
  # is when a half-installed copy is most likely to be left behind. Each item
  # is removed only when it was absent before and is present now, so a run
  # that failed before installing anything has nothing to undo and prints
  # nothing. A failure here is reported and does not replace the exit status
  # that got us here.
  cleanup_brew() {
    if [ "$brew_formula_was" = 0 ] && brew_formula_present; then
      brew uninstall --formula "$brew_formula" >/dev/null 2>&1 \
        || say "release-verify: could not remove $brew_formula — run: brew uninstall $brew_formula"
    fi
    if [ "$brew_tap_was" = 0 ] && brew_tap_present; then
      brew untap "$brew_tap" >/dev/null 2>&1 \
        || say "release-verify: could not remove the tap $brew_tap — run: brew untap $brew_tap"
    fi
    # `brew untap` leaves the trust entry that `brew install` wrote, and
    # untrusting an entry that is not there is harmless.
    if [ "$brew_trusted_was" = 0 ] && brew trust --help >/dev/null 2>&1; then
      brew untrust --formula "$brew_formula" >/dev/null 2>&1 || true
    fi
  }
  # One trap per shell: this replaces the npm check's, so it removes that
  # check's scratch directory as well when there is one.
  trap 'cleanup_brew; [ -z "${install_dir:-}" ] || rm -rf "$install_dir"' EXIT
  for attempt in 1 2 3; do
    # `brew update` first, so a tap cloned on an earlier release hands over
    # the formula for this one. Upgrading a copy that is already current only
    # warns, so the same command serves both cases.
    if { brew update --quiet \
         && if [ "$brew_formula_was" = 1 ]; then brew upgrade "$brew_formula"; else brew install "$brew_formula"; fi; } >/dev/null; then
      break
    fi
    [ "$attempt" -lt 3 ] || die "brew could not install $brew_formula — run: brew install $brew_formula"
    say "release-verify: brew install $brew_formula failed (attempt $attempt) — trying again in 30s"
    sleep 30
  done
  brew_bin="$(brew --prefix)/bin/spoolway"
  got="$("$brew_bin" --version)" \
    || die "the brew-installed $brew_bin would not run"
  case "$got" in
    *"$version"*) ;;
    *)
      tap_version="$(brew info --json=v2 "$brew_formula" | jq -r '.formulae[0].versions.stable // "unknown"')" || tap_version=unknown
      die "brew-installed spoolway reports '$got', not $version. The tap holds $tap_version. If that is not $version, the release workflow's homebrew job did not push this release, so re-run it. The tap only ever holds the newest release, so $version is also not checkable here once a later one is out." ;;
  esac
  if [ "$brew_formula_was" = 1 ]; then
    say "release-verify: brew upgraded $brew_formula to $version and it reports $version, then left it installed"
  else
    say "release-verify: brew installed $brew_formula $version and it reports $version, then removed it"
  fi
  brew_summary="a real brew install checked"
else
  say "release-verify: brew is not on this machine — skipping the brew install check"
fi

say "release-verify: $tag is published — tag on origin, ${#pkgs[@]} platform packages plus the wrapper, $expected release assets, a body matching the tagged section, $install_summary, and $brew_summary"
