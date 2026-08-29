#!/usr/bin/env bash
# A base that moves under a branch on its way to the hand-off.
#
# `handover` is a command step now (`run: spoolway stack`) and never rebases —
# see the plan's own non-goals, and `stack-ancestry` before this task, which
# made the ancestry a stacked pull request needs a fact of the cut rather than
# something rebuilt at hand-off. So the case this suite proves is the opposite
# of what it used to: the base moving under a branch on its way out is a
# non-event. `git push --force-with-lease` leases only the branch's own
# remote-tracking ref, never the base it targets, so the push and the pull
# request both go through untouched — and nothing in the branch reconciles
# with what moved, because GitHub computes a pull request's diff against the
# base's live tip and never needed a local rebase to do it.
#
# `clash` is held at `handover`, the base moves under it with a change to the
# same file, and the lane hands the change over anyway without touching it.
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
configure_project plan/live "$LIVE/worktrees"
publish plan/live

BODY="$LIVE/body.md"
task_body "$BODY"

task_doc "$LIVE/clash.md" clash "$BODY" "group: live" "touches: [src/main.rs]"
must "the task queues" "$SPOOLWAY" queue add --from "$LIVE/clash.md"
# A second rung, queued and never driven. Its only job is to still be open when
# `clash` hands over, so that handover opens a pull request and stops instead of
# going on to land the stack — landing merges the branch back into `plan/live`,
# and the ancestry this suite is about reads backwards after that.
task_doc "$LIVE/spare.md" spare "$BODY" "group: live" \
  "touches: [src/spare.rs]" "depends_on: [clash]"
must "and one above it, to keep the plan open" "$SPOOLWAY" queue add --from "$LIVE/spare.md"

# Driven to the hand-off and no further: its work is committed on its own
# branch, and nothing has been pushed anywhere yet.
if drive_and_hold clash handover 30; then ok "a finished task reaches the handover step"
else bad "a finished task reaches the handover step (at \`$(stage_of clash)\`)"; fi

# The base moves with a change to the same file the lane wrote in its own
# worktree, with different content — the one scenario an actual rebase would
# have to reconcile. Pushed, because what `handover` leases against is the
# forge's own idea of the branch, and a change never pushed anywhere would
# prove nothing.
echo "the base's own idea" > work-clash.txt
must "the base moves" git add work-clash.txt
must "the base moves" git commit -qm "the base moves under clash"
must "and the forge hears about it" git push -q origin plan/live

# Driven to the end of its own pipeline. `spare` is still open, so the handover
# opens the pull request and stops there rather than landing the stack — which
# is the window this suite is about: the base has moved, and nothing has
# merged anything back yet.
if drive_and_hold clash gone 60; then ok "the handover step hands over anyway, without touching what moved"
else bad "the handover step hands over anyway, without touching what moved (at \`$(stage_of clash)\`)"; fi
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
