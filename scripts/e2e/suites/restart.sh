#!/usr/bin/env bash
# The restart guard: a caller in a tight loop against a repo that cannot run
# is eventually refused, rather than restarted forever.
#
# `pipeline check` already refuses a cycle no `loop:` bounds, inside one
# pipeline's own graph. A caller restarting `spoolway dispatch` against a repo
# whose pipeline is broken — or that just keeps another dispatcher held —
# is the same shape one level up: nothing inside a single pass loops, but the
# process outside it does, and the engine has just as little business
# trusting that it will stop on its own. This is the guard that answers it,
# and the exit codes a script needs to tell an ordinary ending from a refusal.
#
# `dispatch.lane_child_ceiling` has no case here: it needs a lane actually
# holding a process open, which this suite's empty-queue and held-lock
# scenarios never do. See `src/dispatch.rs`'s own unit tests for that one.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"
# shellcheck source=../fixture.sh
source "$HERE/../fixture.sh"
# shellcheck source=../agents.sh
source "$HERE/../agents.sh"

new_repo "${WORK:-$(mktemp -d)}/proj"

works "init scaffolds .spoolway" "$SPOOLWAY" init
agent_models
must "the spoolway commit" git add -A
must "the spoolway commit" git commit -qm "spoolway"
must "the plan branch" git checkout -q -b plan/demo

# ------------------------------------------------- starts that cannot run at all
# A live pid stands in for another dispatcher already serving this repo —
# `holder` only asks whether the pid is running, not what it is running.
# `Repo::lock_file` is `dispatch.pid` beside the rest of a project's runtime
# state under `Repo::home`, not the checkout's own `.spoolway/` — the two used
# to be the same directory, and a suite planting the pid in the checkout would
# silently miss the lock check and read straight through to the empty queue.
echo $$ > "$SPOOLWAY_PROJECT_HOME/dispatch.pid"

# Four in a row, each an ordinary "deferred", each its own exit code — not the
# generic 1 an error gets, because a script restarting this in a loop needs to
# tell "deferred" from "the guard has had enough" before the fifth call ever
# happens.
# `--plain`, on every call below: without it a start that finds the lock held
# does not return at all — it becomes a one-shot status watcher and sits
# printing the board until interrupted, which is the right thing for a person
# and a hang for a suite asserting an exit code.
for n in 1 2 3 4; do
  exit_code "start $n could not run and says so, not an error" 4 "$SPOOLWAY" dispatch --plain
done

# The fifth is refused outright: the guard, not the lock check, answers first.
says "a fifth start in the same window is refused" \
  "4 starts in a row could not run" "$SPOOLWAY" dispatch --plain
says "naming the last reason" "already running" "$SPOOLWAY" dispatch --plain
says "and the way out" "spoolway dispatch --force" "$SPOOLWAY" dispatch --plain
exit_code "with its own exit code" 5 "$SPOOLWAY" dispatch --plain

# --force starts one anyway — still deferred, because the lock is still held,
# but no longer refused — and clears the count behind it.
exit_code "--force starts one anyway" 4 "$SPOOLWAY" dispatch --plain --force
exit_code "and the guard is not still tripped on the very next start" 4 \
  "$SPOOLWAY" dispatch --plain

rm -f "$SPOOLWAY_PROJECT_HOME/dispatch.pid"

# ---------------------------------------------------------- an ordinary ending
# An empty queue is not a start that could not run — it is a fact about the
# project, and restarting into one forever is a caller's own choice, never the
# guard's business. Run past the threshold on empty queues alone and nothing
# trips: exit 3 every time, never 5.
for n in 1 2 3 4 5 6; do
  exit_code "an empty queue never counts against the guard (start $n)" 3 \
    "$SPOOLWAY" dispatch --plain
done

finish
