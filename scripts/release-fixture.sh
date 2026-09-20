#!/usr/bin/env bash
# Scaffold the fixture the nightly upgrade suite will start asking for, and
# land it on `main`. Run by the `fixture` step of the release pipeline, after
# `released`.
#
# `scripts/e2e/suites/upgrade.sh` runs a project a *past* release actually
# wrote through the new binary. It asks for `scripts/e2e/fixtures/<version>/`
# once two things are true: `origin` carries `v<version>`, and `Cargo.toml`
# has moved past it. Both are false during the release that cuts <version> and
# both become true at the next bump — so the fixture can only be made after
# the tag, and is only demanded much later. That gap is the whole problem this
# script exists to close. It was documented as a closing step in
# docs/releasing.md and nothing enforced it, so 0.4.0 shipped without one and
# main's daily dress rehearsal was red for two days before anybody read it as a
# missing fixture rather than a regression.
#
# Scaffolded from a binary built at the tag, not from `npx spoolway@<version>`.
# The tree is identical either way — same source, same commit — and the build
# needs only what a release lane already has. scripts/release-verify.sh's own
# header makes the rule: node and npm are not things a release lane's machine
# is required to have.
#
# Idempotent. A fixture already on `main` is success, not a conflict, so a
# re-run after a partial failure costs one `git cat-file`. The question is
# asked of `origin/main` rather than of the lane's own checkout, which was
# cut from the candidate and cannot have the fixture in it.
set -euo pipefail

say() { printf '%s\n' "$*" >&2; }
die() { say "release-fixture: $*"; exit 1; }

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRATCH=

cleanup() {
  [ -n "$SCRATCH" ] || return 0
  git -C "$REPO" worktree remove --force "$SCRATCH/tag" 2>/dev/null || true
  git -C "$REPO" worktree remove --force "$SCRATCH/main" 2>/dev/null || true
  rm -rf "$SCRATCH"
}
trap cleanup EXIT

git -C "$REPO" fetch --quiet --tags --force origin

# The version that was just released, read off `origin/main` rather than the
# lane's own checkout, for the same reason release-verify.sh does: the lane is
# a worktree cut from the candidate, and the release commit landed on main
# after it.
version="$(git -C "$REPO" show origin/main:Cargo.toml \
  | awk '/^\[package\]/{p=1;next} /^\[/{p=0} p && /^version *=/{gsub(/[" ]/,"",$3); print $3; exit}')"
[ -n "$version" ] || die "could not read the package version from origin/main:Cargo.toml"
tag="v$version"

# Without the tag there is nothing to scaffold *from*, and no fixture is owed
# yet either — the suite asks only once the tag is out. `released` runs before
# this and asserts the tag, so reaching here without one means the pipeline was
# driven out of order rather than that the release is fine.
git -C "$REPO" ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null \
  || die "$tag is not on origin — nothing to scaffold a fixture from"

dest="scripts/e2e/fixtures/$version"
if git -C "$REPO" cat-file -e "origin/main:$dest/.spoolway/config.toml" 2>/dev/null; then
  say "release-fixture: $dest/ is already on main — nothing to do"
  exit 0
fi

SCRATCH="$(mktemp -d)"
say "release-fixture: scaffolding $dest/ from $tag"

# That version's own binary, from that version's own tag.
git -C "$REPO" worktree add --quiet --detach "$SCRATCH/tag" "$tag"
( cd "$SCRATCH/tag" && cargo build --quiet ) \
  || die "the tree at $tag does not build; a fixture has to come from that release's own binary"
SPOOLWAY="$SCRATCH/tag/target/debug/spoolway"

# A scratch $HOME so `init` claims a throwaway project rather than registering
# anything real, and a version-check cache seeded as current and freshly
# checked so `release::newer()` never spawns the detached `npm view` child it
# otherwise would. `cache_path()` prefers $XDG_STATE_HOME, so an inherited one
# would point the binary at a real cache outside this scratch $HOME entirely —
# it is pinned here for that reason, exactly as upgrade.sh pins it.
#
# Scoped to the binary rather than exported over the whole script. A blanket
# `export HOME` also takes git's identity away — `user.email` is usually only
# in the real `~/.gitconfig` — and the first thing to notice would have been
# the fixture commit at the bottom refusing to be written.
mkdir -p "$SCRATCH/home/.local/state/spoolway" "$SCRATCH/proj"
printf '{"version":"%s","checked":%s}\n' "$version" "$(date +%s)" \
  >"$SCRATCH/home/.local/state/spoolway/latest.json"
spool() {
  env HOME="$SCRATCH/home" XDG_STATE_HOME="$SCRATCH/home/.local/state" "$SPOOLWAY" "$@"
}

cd "$SCRATCH/proj"
git init --quiet .
printf '# scratch\n' >README.md
git add -A && git commit --quiet -m scratch
spool init >/dev/null || die "$tag's own binary could not init a scratch project"

# One value set with that same binary, under whatever table that release calls
# it — the point of the fixture is to carry a real setting across the upgrade,
# so it is set by the old binary rather than written into the file here.
spool config set housekeeping.retention_days 45 >/dev/null \
  || die "$tag's own binary could not set housekeeping.retention_days"

# One line of prose below the generated key block, for `spoolway sync` to
# preserve. Its wording matches the fixtures already in the tree, because
# upgrade.sh compares the bytes either side of the block and nothing else.
python3 - <<'PY'
import pathlib, sys
p = pathlib.Path(".spoolway/pipelines/default.yml")
text = p.read_text()
marker = "# <<< spoolway <<<\n"
if text.count(marker) != 1:
    sys.exit(f"expected exactly one {marker.strip()!r} in {p}, found {text.count(marker)}")
p.write_text(text.replace(
    marker,
    marker + "#\n# a note this project added below the fence; upgrading must leave it exactly here\n"))
PY

# Committed in a worktree of its own so the lane's checkout is never touched,
# and pushed straight to main the way the release commit before it was.
git -C "$REPO" worktree add --quiet --detach "$SCRATCH/main" origin/main
# Cleared rather than merged into. The guard above only proves there is no
# `config.toml` there; a half-written fixture from an interrupted run would
# otherwise take the copy one level deeper and produce `.spoolway/.spoolway`.
rm -rf "$SCRATCH/main/$dest"
mkdir -p "$SCRATCH/main/$dest"
cp -a "$SCRATCH/proj/.spoolway" "$SCRATCH/main/$dest/.spoolway"
git -C "$SCRATCH/main" add -- "$dest"
git -C "$SCRATCH/main" commit --quiet -m "test(e2e): scaffold $version's upgrade fixture

Scaffolded by $tag's own binary, at $tag, by scripts/release-fixture.sh. The
nightly upgrade suite asks for it from the first bump past $version."

git -C "$SCRATCH/main" push --quiet origin "HEAD:main" \
  || die "could not push $dest/ to main — main has moved since this step started; re-run it"

say "release-fixture: $dest/ is on main, scaffolded by $tag's own binary"
