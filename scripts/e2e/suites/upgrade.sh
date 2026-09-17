#!/usr/bin/env bash
# Does the current binary still read what an older release wrote?
#
# Every other suite builds its project from a seed written inline — see
# `fixture.sh`'s own header for why that is right for everything else. It is
# wrong for exactly one question: whether `spoolway update` still carries an
# *actual* old project forward, config values and hand-written prose alike.
# A seed built by today's binary can only ever be current; the only way to
# get something genuinely behind is to have really been that old release
# once. So `scripts/e2e/fixtures/<version>/.spoolway/` is not written here —
# each one is a `.spoolway/` tree that version's own `spoolway init` actually
# scaffolded, at that version's own tag. One config value was then set with
# that same binary's own `config set`: under the still-current `[retention]`
# table at 0.1.0, and directly under `[housekeeping]` at 0.2.0, since the fold
# into it had already happened by then. One line of prose was hand-added
# below the pipeline file's generated key block in each, for `spoolway
# update` to preserve.
#
# No `covers:` tag of its own, for the same reason `overrides.sh` carries
# none: `housekeeping.retention_days` and the pipeline key block are already
# regular settings with their own coverage: what is being asked here is
# whether *moving* to them across a real upgrade still lands the value,
# which is a question about `Config::migrate` and `spoolway update`, not
# about either setting.
#
# nightly only — see run.sh's own header for why, and the task's own goal:
# this is a release-time question ("does this binary still read what a past
# release wrote"), not a per-chain one. The `pr` tier is what the `suite`
# step runs, once, on the last task of a chain, through
# `scripts/e2e-pr.sh` and under a 45-minute timeout — and an ordinary
# chain's diff essentially never touches the fold in `Config::migrate` or a
# shipped pipeline's key block. So leaving `upgrade` out of `pr_suites`
# costs that gate nothing. The `nightly` tier still asks the question: once
# a day against main, and again from the release workflow before every tag,
# which is when an answer here is actually worth having.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../../.." && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"
# shellcheck source=../fixture.sh
source "$HERE/../fixture.sh"

WORK=${WORK:-$(mktemp -d)}
FIXTURES="$HERE/../fixtures"

# ------------------------------------------------------- the plan stays honest
#
# A released version with no fixture is a version this suite has quietly
# stopped answering for. Caught here, by name, rather than by the suite going
# on to say nothing about it.
#
# A fixture is asked for once two things are true of a `CHANGELOG.md` section's
# version: `origin` carries its `v<version>` tag, and `Cargo.toml` has moved
# past it. Either one alone is a check that is false only *while* a version is
# being cut, which is the family `ci.yml`'s dress-rehearsal job exists to
# catch:
#
#   The tag alone would red the publication itself. A fixture is a
#   `.spoolway/` tree that version's own `spoolway init` scaffolded *at that
#   version's own tag* (see this file's header), so it is scaffolded from the
#   published release — the closing step of `docs/releasing.md` — while the
#   release workflow runs this suite again from the tag it has just pushed.
#   Asking there demands something that does not exist yet.
#
#   `Cargo.toml` alone — the rule this carried before — exempts only the one
#   version being cut, which is not the same version in every job that runs
#   this suite. `ci.yml`'s dress-rehearsal bumps `Cargo.toml` to the next
#   minor precisely so these checks run at a version the repo does not have,
#   and inside that job the untagged release commit's own version is no longer
#   the exempt one, so it was asked for a fixture no tag existed to scaffold.
#   That reddened main's daily gate for the whole of 0.4.0's release window,
#   and 0.3.0's rehearsal before it.
#
# Together they ask for exactly the versions that are out and moved past, so
# nothing is retired: a release whose closing fixture was never scaffolded is
# still caught, now by the next dress-rehearsal rather than by the next real
# bump. And the moment an exempt version's fixture does land it is checked like
# any other, so scaffolding one straight after the tag needs no change here.
#
# Read off `origin` rather than off `git tag`: CI checks out a single commit
# without tags, so a local-tag question would answer "nothing needed" for every
# version at once and retire the check altogether. `ls-remote` asks the same
# question from a tagless checkout, needs no credentials against a public
# repository, and fetches nothing. If it cannot be reached at all, this falls
# back to the `Cargo.toml`-only rule, which asks for *more* fixtures rather
# than fewer — an unreachable remote must not be a way to be asked nothing.
RELEASING=$(awk -F'"' '/^version = /{print $2; exit}' "$REPO/Cargo.toml")
if TAGGED=$(GIT_TERMINAL_PROMPT=0 timeout 60 \
    git -C "$REPO" ls-remote --tags --refs origin 'refs/tags/v*' 2>/dev/null); then
  TAGGED=$(awk '{sub(/^refs\/tags\/v/, "", $2); print $2}' <<<"$TAGGED")
else
  TAGGED=
  UNREACHABLE=1
  printf '  \033[33mnote\033[0m  %s\n' \
    "origin was not reachable for the tag list — asking for every fixture but $RELEASING's"
fi

# asked_for <version> — is that version out, and is the repo past it?
asked_for() {
  [ "$1" != "$RELEASING" ] || return 1
  [ -z "${UNREACHABLE:-}" ] || return 0
  grep -qxF -- "$1" <<<"$TAGGED"
}

while read -r version; do
  [ -n "$version" ] || continue
  if ! asked_for "$version" && [ ! -d "$FIXTURES/$version/.spoolway" ]; then
    if [ "$version" = "$RELEASING" ]; then
      why="Cargo.toml still names $version"
    else
      why="origin carries no v$version tag"
    fi
    printf '  \033[33mnote\033[0m  %s\n' \
      "scripts/e2e/fixtures/$version/ is not asked for while $why — scaffold it from the v$version tag once that tag is out; the first bump past it makes this a check"
    continue
  fi
  works "scripts/e2e/fixtures/$version/ was scaffolded for the $version release" \
    test -d "$FIXTURES/$version/.spoolway"
done < <(grep -oE '^## [0-9]+\.[0-9]+\.[0-9]+$' "$REPO/CHANGELOG.md" | awk '{print $2}')

# byte_range <file> <bytes>
#
# The first `bytes` bytes of `file`, raw. Used for the prose *before* the
# generated block.
byte_range() {
  head -c "$2" "$1"
}

# offset_of <file> <marker-line>
#
# The 0-based byte offset the given whole comment line starts at, or empty if
# it is not there. `-x` matches only a whole line, so a marker string that
# happened to appear embedded in some other, longer line would not be found
# here in its place.
offset_of() {
  grep -abxF -- "$2" "$1" | head -1 | cut -d: -f1
}

# tail_from_marker <file> <marker-line>
#
# Everything in `file` from the byte immediately after `marker-line`'s own
# trailing newline to EOF, raw. Used for the prose *after* the generated
# block: the marker text itself never changes, so both files' copies of this
# range start at the same logical point even though the block between the two
# markers is a different length in each.
tail_from_marker() {
  local off
  off=$(offset_of "$1" "$2") || return 1
  tail -c +"$((off + ${#2} + 2))" "$1"
}

# Both raw byte ranges rather than anything reconstructed line by line: a
# `diff` or `awk` pass over matched lines would print its own trailing
# newline regardless of what the file actually ended on, which is exactly the
# kind of difference — a generated file rewritten one byte short — this
# suite exists to catch. `cmp` on the untouched bytes cannot mask that.
BEGIN_MARKER="# >>> spoolway >>>"
END_MARKER="# <<< spoolway <<<"

# byte_for_byte_outside_block <what> <before> <after>
byte_for_byte_outside_block() {
  local what=$1 before=$2 after=$3
  local before_head after_head
  before_head=$(offset_of "$before" "$BEGIN_MARKER")
  after_head=$(offset_of "$after" "$BEGIN_MARKER")
  works "$what — the prose above the key block" \
    cmp <(byte_range "$before" "$before_head") <(byte_range "$after" "$after_head")
  works "$what — the prose below the key block" \
    cmp <(tail_from_marker "$before" "$END_MARKER") <(tail_from_marker "$after" "$END_MARKER")
}

# stage <version>
#
# A fresh repo with that version's own fixture laid over it, registered
# under a scratch $HOME so `spoolway update` — which refuses an
# unregistered project — has something to run against. `spoolway init`
# writes only what the fixture does not already have (see its own `place`),
# so this claims the project without touching a byte the fixture carries.
#
# A version-check cache is seeded first, stamped as current and freshly
# checked: unseeded, `release::newer()` finds no cache, decides it is stale,
# and spawns a detached child that shells out to `npm view` — which nothing
# here should ever do, and would run past this suite's own lifetime besides.
# Stamping it with this binary's own version rather than a placeholder one
# also keeps `spoolway update`'s own upgrade check honest: a placeholder
# `newer()` believed was newer would send it into a real `npm install -g`.
#
# `release::cache_path()` reads `$XDG_STATE_HOME/spoolway/latest.json` in
# preference to `$HOME/.local/state/...` — and an inherited `XDG_STATE_HOME`
# from the machine running this suite would then point the binary at a real,
# unseeded cache outside this fixture's scratch $HOME entirely, npm call and
# all. So `XDG_STATE_HOME` is pinned here to a directory under the same
# scratch $HOME `new_repo` just set, and the cache is written to the exact
# path that pin makes `cache_path()` resolve to.
stage() {
  local version=$1
  local dir="$WORK/$version/proj"
  new_repo "$dir"
  export XDG_STATE_HOME="$HOME/.local/state"
  local cache="$XDG_STATE_HOME/spoolway/latest.json"
  mkdir -p "$(dirname "$cache")"
  printf '{"version":"%s","checked":%s}\n' "$("$SPOOLWAY" --version | awk '{print $2}')" \
    "$(date +%s)" >"$cache"
  cp -a "$FIXTURES/$version/.spoolway" "$dir/.spoolway"
  must "the fixture committed" git -C "$dir" add -A
  must "the fixture committed" git -C "$dir" commit -qm "fixture $version"
  must "the project claims its state directory" "$SPOOLWAY" init
}

# ----------------------------------------------------------------- 0.1.0: fold
#
# `[update]`, `[calibrate]` and `[retention]` are current tables at 0.1.0, not
# legacy ones — `retention.days` was set to 90 with that release's own
# `config set`, against its own default of 30, before the tree was committed.
# The pipeline file's key block is 0.1.0's own too, and differs from what
# this binary ships: it still carries a `cleanup:` line the block has since
# dropped.
stage 0.1.0
PIPELINE=".spoolway/pipelines/default.yml"
cp "$PIPELINE" "$WORK/0.1.0/before-default.yml"
has "the 0.1.0 fixture really does carry that release's own stale key block" \
  "cleanup           true" "$WORK/0.1.0/before-default.yml"

must "spoolway update runs against the 0.1.0 project" "$SPOOLWAY" update

has "the value set under the retired [retention] table reached [housekeeping]" \
  "retention_days = 90" .spoolway/config.toml
lacks "the retired [retention] table itself is gone" "[retention]" .spoolway/config.toml
lacks "the retired [update] table itself is gone" "[update]" .spoolway/config.toml
lacks "the retired [calibrate] table itself is gone" "[calibrate]" .spoolway/config.toml
lacks "the stale key block's retired \`cleanup\` line is gone, not left alone" \
  "cleanup           true" "$PIPELINE"
has "the key block now reads the way this binary ships it" \
  "on_fail\` routes it later" "$PIPELINE"
byte_for_byte_outside_block "the prose around the refreshed block came back byte for byte" \
  "$WORK/0.1.0/before-default.yml" "$PIPELINE"

# ------------------------------------------------------------- 0.2.0: no-op
#
# By 0.2.0 the fold had already happened — `[housekeeping]` is the only table
# there is — and its pipeline key block already reads the way this binary
# ships it (`git diff v0.2.0..HEAD -- assets/pipelines/default.yml` is
# empty), so an update over it should carry its `retention_days` forward
# unchanged rather than migrate or rewrite anything.
stage 0.2.0
PIPELINE=".spoolway/pipelines/default.yml"
cp "$PIPELINE" "$WORK/0.2.0/before-default.yml"

must "spoolway update runs against the 0.2.0 project" "$SPOOLWAY" update

has "the housekeeping value already in place survives the update" \
  "retention_days = 45" .spoolway/config.toml
byte_for_byte_outside_block "the prose around the already-current block is untouched" \
  "$WORK/0.2.0/before-default.yml" "$PIPELINE"

finish
