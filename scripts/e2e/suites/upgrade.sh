#!/usr/bin/env bash
# Does the current binary still read what 0.6.0 wrote?
#
# Every other suite builds its project from a seed written inline — see
# `fixture.sh`'s own header for why that is right for everything else. It is
# wrong for exactly one question: whether `spoolway sync` still carries an
# *actual* old project forward, config values and hand-written prose alike.
# A seed built by today's binary can only ever be current; the only way to
# get something genuinely behind is to have really been that old release
# once. So `scripts/e2e/fixtures/0.6.0/.spoolway/` is not written here — it
# is a `.spoolway/` tree 0.6.0's own `spoolway init` actually scaffolded, at
# 0.6.0's own tag, with `housekeeping.retention_days` then set by that same
# binary's own `config set` and one line of prose hand-added below the
# pipeline file's generated key block, for `spoolway sync` to preserve.
#
# 0.6.0 is the oldest version spoolway upgrades from — see the
# `drop-pre-0-6-migrations` task. A project set up by anything older is set
# up again with `spoolway init`, not carried forward, so there is nothing
# for this suite to stage below that floor and no fixture for it either.
#
# No `covers:` tag of its own, for the same reason `overrides.sh` carries
# none: `housekeeping.retention_days` and the pipeline key block are already
# regular settings with their own coverage: what is being asked here is
# whether *moving* to them across a real upgrade still lands the value,
# which is a question about `spoolway sync`, not about either setting.
#
# nightly only — see run.sh's own header for why, and the task's own goal:
# this is a release-time question ("does this binary still read what a past
# release wrote"), not a per-chain one. A `suite` step runs its tier once, on
# the last task of a chain, under a 45-minute timeout: the `pr` tier through
# `scripts/e2e-pr.sh` in `impl`, and the narrower `smoke` tier through
# `scripts/e2e-smoke.sh` in `impl_lite`. Neither carries `upgrade`, and an
# ordinary chain's diff essentially never touches `spoolway sync`'s own
# upgrade step or a shipped pipeline's key block. So leaving `upgrade`
# out of `pr_suites` costs either gate nothing. The `nightly` tier still asks the question: once
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

# The oldest version this suite still carries a fixture for, and the oldest
# one it ever asks for one below. Below it, a `CHANGELOG.md` section earns no
# note and no check: those releases are not upgraded from any more, and
# `scripts/e2e/fixtures/` below this floor is retired along with the code
# that carried them forward.
FLOOR="0.6.0"

# below_floor <version> — is this version older than `FLOOR`?
#
# `sort -V` rather than a string or numeric compare: every version here is
# `major.minor.patch` and nothing heavier, but a plain string compare reads
# "0.10.0" as older than "0.6.0", and `sort -V` is the one comparison this
# repository already trusts to get that right (see `ci.yml`'s own release
# checks).
below_floor() {
  [ "$1" != "$FLOOR" ] && [ "$(printf '%s\n%s\n' "$1" "$FLOOR" | sort -V | head -1)" = "$1" ]
}

# ------------------------------------------------------- the plan stays honest
#
# A released version at or above `FLOOR`, with no fixture, is a version this
# suite has quietly stopped answering for. Caught here, by name, rather than
# by the suite going on to say nothing about it. A version below `FLOOR` is
# never asked for at all — it predates the upgrade floor, and carries no
# fixture by design.
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
  below_floor "$version" && continue
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
# under a scratch $HOME so `spoolway sync` — which refuses an
# unregistered project — has something to run against. `spoolway init`
# writes only what the fixture does not already have (see its own `place`),
# so this claims the project without touching a byte the fixture carries.
#
# A version-check cache is seeded first, stamped as current and freshly
# checked: unseeded, `release::newer()` finds no cache, decides it is stale,
# and spawns a detached child that shells out to `npm view` — which nothing
# here should ever do, and would run past this suite's own lifetime besides.
# `notify()` runs ahead of `init` and `sync` alike, so both need the cache
# seeded, even though neither one is `update` and neither ever reaches
# `release::upgrade`'s own live npm call.
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
  # Captured rather than let straight through: acceptance criterion 3 — a
  # second `init` over a project that already has everything reports `kept`
  # for every file it considered and writes nothing, and this is the one
  # place in the suite that runs `init` a second time over a tree it did
  # not just scaffold itself, so it is the one place that can prove `place`
  # reaches a *real* past release's own files rather than only ones this
  # suite's own fixtures were built by today's binary.
  if "$SPOOLWAY" init --yes >"$WORK/$version/init.out" 2>&1; then
    :
  else
    printf '  \033[31mSETUP\033[0m the project claims its state directory\n' >&2
    sed 's/^/        /' "$WORK/$version/init.out" >&2
    annotate "setup failed: the project claims its state directory"
    exit 2
  fi
}

# assert_init_reused_everything <version>
#
# Review finding 1: `stage`'s own `spoolway init` runs a second time over a
# tree that release's own `init` already scaffolded in full, so every file
# it considers is already there — this asserts the transcript `stage`
# captured reads `kept` for it rather than `wrote`. A fixture carries only
# its `.spoolway/`, never the skills a release's `init` installed beside it,
# so this run installs them for the first time — and since the
# `home-mode-messages` task, a run that wrote skills closes on the install
# report alone, never also on `nothing to install.` beside it. Every
# fixture under `scripts/e2e/fixtures/` ships with `issue_tracking.hook`
# blank, so there is no
# `.spoolway/hooks/` row to expect here either — that
# half of acceptance criterion 3 (a `wrote` row for something a fixture
# actually lacks) is `tests/init_output.rs`'s own job, against a project
# this binary controls the shape of rather than a fixture frozen at a past
# release.
assert_init_reused_everything() {
  local version=$1 out="$WORK/$1/init.out"
  lacks "the $version fixture's second init wrote anything at all" "  wrote" "$out"
  has "and reported every file it considered kept instead" \
    "  kept     .spoolway/config.toml" "$out"
  has "and installed the skills the fixture never carried" \
    "Skills installed successfully, into .claude/skills." "$out"
  lacks "without also claiming there was nothing to install" \
    "nothing to install." "$out"
}

# ---------------------------------------------------------- 0.6.0: the floor
#
# The oldest release this binary still upgrades from. Its own `spoolway
# init` wrote `dispatch.worktree_root` and `issue_tracking.on_fail` blank —
# neither was retired yet — and its own `config set` put
# `housekeeping.retention_days` at 45, against the shipped default of 30.
# The pipeline file's key block is 0.6.0's own too, byte for byte what this
# binary still ships, so the only change `sync` should make to it at all is
# the one line of hand-added prose riding below the fence unchanged.
stage 0.6.0
assert_init_reused_everything 0.6.0
PIPELINE=".spoolway/pipelines/default.yml"
cp "$PIPELINE" "$WORK/0.6.0/before-default.yml"

must "spoolway sync runs against the 0.6.0 project" "$SPOOLWAY" sync

has "the housekeeping value the 0.6.0 fixture set survives the sync" \
  "retention_days = 45" .spoolway/config.toml
byte_for_byte_outside_block \
  "the prose around the key block came back byte for byte — nothing in it changed" \
  "$WORK/0.6.0/before-default.yml" "$PIPELINE"

# The project loads clean, with nothing left for `pipeline check` to refuse —
# the fixture configures no agents, so the one thing left to say is the same
# missing-model complaint every other fixture in this suite draws, proof the
# pipelines loaded far enough for the per-step checks to run at all.
says "the 0.6.0 project's pipelines load clean after the sync" \
  "names no model" \
  "$SPOOLWAY" pipeline check

# And the queue itself still runs: a task queues against the upgraded
# project's own `default` pipeline, and the queue screen finds it there —
# the one piece of this suite that is not about `sync` or `config.toml` at
# all, but about the upgraded project actually being able to do its job
# afterward. A plan branch first, the same as every other suite's
# `configure_project` ends on, now that the sync and `pipeline check`
# assertions above — which do not care which branch they run on — are done
# with `main`.
must "a plan branch for the queued task" git checkout -q -b plan/upgrade
BODY="$WORK/0.6.0/body.md"
task_body "$BODY"
task_doc "$WORK/0.6.0/queued.md" upgrade-queued "$BODY" "group: upgrade"
must "a task queues against the upgraded project" \
  "$SPOOLWAY" queue add --from "$WORK/0.6.0/queued.md"
says "and the queue screen finds it there" \
  "upgrade-queued" \
  "$SPOOLWAY" queue list

finish
