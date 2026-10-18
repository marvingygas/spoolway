#!/usr/bin/env bash
# A base that moves under a branch on its way to the hand-off.
#
# `create-pr` never rebases and never force-pushes. So the case this suite
# proves is that the base moving under a branch on its way out is a
# non-event: the push and the pull request both go through untouched, and
# nothing in the branch reconciles with what moved, because GitHub computes a
# pull request's diff against the base's live tip and never needed a local
# rebase to do it.
#
# `clash` is held after `document`, immediately before `create-pr`; the base
# moves under it with a change to the same file, and the step hands the
# change over anyway without touching it.
#
# This is the one scenario that wants a real conflict, which is why every other
# suite's stand-in agent writes to a file of the task's own.
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
configure_project plan/live
publish plan/live

BODY="$LIVE/body.md"
task_body "$BODY"

task_doc "$LIVE/clash.md" clash "$BODY" "group: live" \
  "gate_at: document"
must "the task queues" "$SPOOLWAY" queue add --from "$LIVE/clash.md"

# Driven through `document` and held before the final step: its
# work is committed on its own branch, and nothing has been pushed anywhere
# yet. Waiting for `create-pr` itself is a race now that it is the final step:
# a fast step can run and archive the task between two polling reads.
if drive_and_hold clash paused 60; then ok "a finished task is held before the create-pr step"
else bad "a finished task is held before the create-pr step (at \`$(stage_of clash)\`)"; fi

# The base moves with a change to the same file the lane wrote in its own
# worktree, with different content — the one scenario an actual rebase would
# have to reconcile. Pushed, because the forge's own idea of the base is what
# a pull request is computed against, and a change never pushed anywhere
# would prove nothing.
echo "the base's own idea" > work-clash.txt
must "the base moves" git add work-clash.txt
must "the base moves" git commit -qm "the base moves under clash"
must "and the forge hears about it" git push -q origin plan/live

# Spend the one-shot schedule that held `document`'s report. With the
# dispatcher still down, this advances the task to `create-pr` without giving
# that step a chance to run until the drive below starts it deliberately.
must "the held task resumes into create-pr" "$SPOOLWAY" resume clash

# Driven to the end of its own pipeline. The step opens the pull request and
# stops there — the window this suite is about: the base has moved, and nothing
# has merged anything back.
if drive_and_hold clash gone 180; then ok "the create-pr step hands over anyway, without touching what moved"
else bad "the create-pr step hands over anyway, without touching what moved (at \`$(stage_of clash)\`)"; fi
if grep -q "rebased onto\|rebase" $SPOOLWAY_PROJECT_HOME/archive/clash.md; then
  bad "and does not claim a rebase that never happened"
  grep "rebased onto\|rebase" $SPOOLWAY_PROJECT_HOME/archive/clash.md | sed 's/^/        /'
else
  ok "and does not claim a rebase that never happened"
fi

if handed_over clash; then ok "and the change really reached the forge"
else bad "and the change really reached the forge"; ls "$FORGE/prs" | sed 's/^/        /'; fi

# The push leases only the branch's own remote-tracking ref, never the base it
# targets — so the moved base neither blocks the push nor rewrites it. The
# branch on the forge still holds exactly what `clash` wrote, and nothing of
# what moved underneath it.
if [ "$(git -C "$FORGE/origin.git" show task/clash:work-clash.txt)" = "work for clash at implement" ]; then
  ok "the branch handed over still holds only its own work, untouched by what moved"
else
  bad "the branch handed over still holds only its own work, untouched by what moved"
  git -C "$FORGE/origin.git" show task/clash:work-clash.txt | sed 's/^/        /'
fi

works "and the task finished, archived out of the queue" \
  test -f $SPOOLWAY_PROJECT_HOME/archive/clash.md

finish
