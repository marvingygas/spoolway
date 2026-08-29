#!/usr/bin/env bash
# The forge itself, and what happens when a lane hands nothing over.
#
# Every pipeline goes through a forge now — there is no route that lands a
# change without a remote — so "landing through a pull request" is no longer a
# mode worth its own suite. `flow` drives the ordinary case and `stacking`
# drives the chained one. What is left here is the part neither of those can
# say anything about: a hand-off that does *not* hand its change over.
#
# That is a real shape rather than a stub's quirk. A project whose hand-off prompt
# pushes and opens the pull request but leaves the merge to a person is exactly
# the flat hand-off case the design keeps room for, and the fact under test is
# that the pipeline does not care: the task finishes either way, and what is or
# is not on the forge afterwards is that prompt's business.
#
# The forge is local: a bare repo on disk as `origin`, plus the `gh` test double
# in scripts/e2e-fake-gh.sh. No repo created and deleted around every run, no
# token, no network. What it costs is that GitHub's own API is not exercised,
# which is deliberate — `scripts/e2e-sandbox.sh --remote` is where the real
# thing goes.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"
# shellcheck source=../fixture.sh
source "$HERE/../fixture.sh"
# shellcheck source=../agents.sh
source "$HERE/../agents.sh"


LIVE=${WORK:-$(mktemp -d)}
CTL="$LIVE/ctl"

new_forge "$LIVE/forge"
install_agents "$LIVE/bin" "$CTL" "" "" "" "$FORGE"

new_repo "$LIVE/proj"
configure_project plan/live "$LIVE/worktrees"
publish plan/live

BODY="$LIVE/body.md"
task_body "$BODY"

# --------------------------------------------- the ordinary hand-off, once
task_doc "$LIVE/shipped.md" shipped "$BODY" "group: live" "touches: [src/main.rs]"
must "a task" "$SPOOLWAY" queue add --from "$LIVE/shipped.md"

if drive shipped gone 90; then ok "a task hands its change over and is archived"
else bad "a task hands its change over and is archived (at \`$(stage_of shipped)\`)"; fi

if handed_over shipped; then ok "the branch reached the remote and a pull request is open"
else bad "the branch reached the remote and a pull request is open"; ls "$FORGE/prs" | sed 's/^/        /'; fi

# The hand-off prompt opens this task's pull request once, inside the one
# `handover` lane, and stops. A second pull request for one branch is a stack
# with a phantom rung in it, so the prompt is told to look before it opens
# one and the double refuses a duplicate the way `gh` does.
opened=$(grep -lx "head=task/shipped" "$FORGE"/prs/[0-9]* 2>/dev/null | wc -l)
if [ "$opened" -eq 1 ]; then
  ok "one branch has exactly one pull request, however often the step ran"
else
  bad "one branch has $opened pull requests"
  grep -H '^head=' "$FORGE"/prs/[0-9]* | sed 's/^/        /'
fi

# ------------------------------------------- a lane that hands nothing over
# `handover` is `spoolway stack` now, and it always pushes and opens the pull
# request once it runs — there is no config key or flag that makes it choose
# to withhold one (see the plan's own non-goals), and by the point it could
# fail on anything past the diff, the push and the pull request are already
# done. The one case left where nothing really reaches the forge is having
# nothing to send: an empty three-dot diff against the cut point refuses
# before either happens, exactly per this task's own acceptance criteria.
# Forced here by resetting the worktree back to what it was cut from, right
# where the task is held before `handover` runs — `spoolway stack` refuses,
# and its `on_fail` routes straight to `blocked` now, the same as any other
# refusal: there is no fallback lane left to decide the empty diff is fine
# and wave the task through on its own, so it parks for a person instead.
task_doc "$LIVE/handed.md" handed "$BODY" "group: live" "touches: [src/main.rs]"
must "a task whose branch will end up empty" "$SPOOLWAY" queue add --from "$LIVE/handed.md"

if drive_and_hold handed handover 90; then ok "it reaches the hand-off with its own work still on the branch"
else bad "it reaches the hand-off with its own work still on the branch (at \`$(stage_of handed)\`)"; fi
must "the branch is wound back to its cut point, leaving nothing to hand over" \
  git -C "$LIVE/worktrees/task-handed" reset -q --hard plan/live

if drive handed blocked 90; then ok "a lane with nothing to hand over blocks for a person instead of guessing"
else bad "a lane with nothing to hand over blocks for a person instead of guessing (at \`$(stage_of handed)\`)"; fi

has "and the command log says why, rather than a bare git failure" \
  "three-dot diff is empty" "$SPOOLWAY_PROJECT_HOME/commands/handed · handover.log"

if handed_over handed; then
  bad "something handed the change over after all"
  cat "$FORGE"/prs/[0-9]* | sed 's/^/        /'
else
  ok "and nothing about it reached the forge"
fi

# ------------------------------------------------- what the double could not do
# The double waves through anything it does not implement and writes it down,
# because a gap in a test stub must never be what stops a run. A run with
# nothing in this log is the honest outcome; anything in it is worth reading.
if [ -s "$FORGE/warnings.log" ]; then
  printf '  note  the gh double logged:\n'
  sed 's/^/          /' "$FORGE/warnings.log" | head -10
fi

finish
