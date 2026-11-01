#!/usr/bin/env bash
# `spoolway queue unqueue`, the command form of the board's `unqueue`.
#
# The move itself — carrying a document back to pending with every reserved
# key stripped — is `status::unqueue_in`, already proven against the board
# by `board-pause.sh`'s own unqueue case. What is only true of the command, and
# needs a real process to prove, is everything a script gets that a keypress
# does not: `--help` actually listing the word, `queue remove` actually
# getting nothing but clap's own unknown-subcommand error, a refusal that
# names a route which actually works, the command actually deleting a real
# worktree and branch for a stopped task and carrying every task after it, with
# a dispatcher running or not, and `--force` and `--all` actually gone — not a
# description of any of that, the thing itself.
#
# No `covers:` tag — the coverage map enumerates `config.toml` keys and
# pipeline step keys, and a queue subcommand is neither.
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

install_agents "$LIVE/bin" "$CTL"
new_repo "$LIVE/proj"
configure_project plan/unqueue

BODY="$LIVE/body.md"
task_body "$BODY"

# ------------------------------------------------------- the round trip out and back
# A task still on `queued` goes back to pending with every task after it in
# its group, as the board's unqueue does, and says one `unqueued` line each.
task_doc "$LIVE/base.md" base "$BODY" "group: unq"
must "base queues" "$SPOOLWAY" queue add --from "$LIVE/base.md"
task_doc "$LIVE/dependent.md" dependent "$BODY" "group: unq" \
  "depends_on: [base]"
must "a sibling that depends on it queues too" "$SPOOLWAY" queue add --from "$LIVE/dependent.md"

BASE_OUT="$LIVE/unqueue-base.out"
if "$SPOOLWAY" queue unqueue base >"$BASE_OUT" 2>&1; then
  ok "unqueuing base carries dependent with it"
else
  bad "unqueuing base carries dependent with it"
  sed 's/^/        /' "$BASE_OUT"
fi
has "it says base moved" "unqueued \`base\` → " "$BASE_OUT"
has "and that dependent moved" "unqueued \`dependent\` → " "$BASE_OUT"
lacks "it deleted nothing, so says no branch" "deleted branch" "$BASE_OUT"
works "base landed in pending" test -f "$SPOOLWAY_PROJECT_HOME/pending/base.md"
works "dependent landed in pending with it" \
  test -f "$SPOOLWAY_PROJECT_HOME/pending/dependent.md"
works "neither is in the queue any more" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/queue/base.md"

must "the round-tripped task still passes the task contract" \
  "$SPOOLWAY" task contract --from "$SPOOLWAY_PROJECT_HOME/pending/base.md"
must "queue add --from takes it again unchanged" \
  "$SPOOLWAY" queue add --from "$SPOOLWAY_PROJECT_HOME/pending/base.md"
must "and its dependent with it" \
  "$SPOOLWAY" queue add --from "$SPOOLWAY_PROJECT_HOME/pending/dependent.md"
works "both are back in the queue" test -f "$SPOOLWAY_PROJECT_HOME/queue/base.md"

# ------------------------------------------------------------------- --help, and no alias
says "queue --help lists unqueue" "unqueue" "$SPOOLWAY" queue --help
refuses "queue remove gets nothing but clap's own unknown-subcommand error" \
  "unrecognized subcommand" "$SPOOLWAY" queue remove base

# --------------------------------------------------------- unknown id and no queued task
refuses "unqueuing a task the queue does not have" "no queued task" \
  "$SPOOLWAY" queue unqueue nope

# ------------------------------------------------------- a task that has started
# A real lane, hung mid-turn, so the task the refusal names is really running
# and the teardown after `pause` is a real teardown, not a description of it.
echo hang > "$CTL/solo"
# A group of its own: `base` and `dependent` are still queued in `unq`, and a
# group is one chain, so an unrelated task there would be a second root.
task_doc "$LIVE/solo.md" solo "$BODY" "group: solo"
must "a task whose lane will hang mid-turn" "$SPOOLWAY" queue add --from "$LIVE/solo.md"

if drive solo implement 120; then ok "it reaches implement and sits there"
else bad "it reaches implement and sits there (at \`$(stage_of solo)\`)"; fi
LANE_PID=$(lane_pid "solo · implement" 30)
if [ -n "$LANE_PID" ]; then ok "the lane really is mid-turn"
else bad "the lane really is mid-turn"; fi
SOLO_WORKTREE=$(worktree_of solo)
if [ -n "$SOLO_WORKTREE" ] && [ -d "$SOLO_WORKTREE" ]; then
  ok "and it really did cut a worktree"
else
  bad "and it really did cut a worktree (worktree_path: \"$SOLO_WORKTREE\")"
fi

# Nothing carries `solo` past `implement` on its own from here: the lane is
# genuinely hung and never reports, so the refusal and the teardown
# below race nothing.
# A running task is refused while the suite's own dispatcher holds the run
# lock: the dispatcher re-reads the queue every pass and may be moving it.
UNQUEUE_OUT="$LIVE/unqueue-refused.out"
if "$SPOOLWAY" queue unqueue solo >"$UNQUEUE_OUT" 2>&1; then
  bad "a running task is refused"
  sed 's/^/        /' "$UNQUEUE_OUT"
else
  ok "a running task is refused"
fi
has "naming the step it is running" "\`solo\` is running step implement" "$UNQUEUE_OUT"
has "and that unqueue takes a stopped task only" "unqueue takes a stopped task only." "$UNQUEUE_OUT"
has "the route that stops it where it stands" "spoolway queue pause solo" "$UNQUEUE_OUT"
has "and the unqueue to run after it" "spoolway queue unqueue solo" "$UNQUEUE_OUT"
has "and says plainly that nothing moved" "nothing was changed." "$UNQUEUE_OUT"
works "the refusal changed nothing: still queued" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/solo.md"
if kill -0 "$LANE_PID" 2>/dev/null; then ok "and the lane is still running"
else bad "and the lane is still running"; fi

# The route the refusal named actually works. `pause` first, over a copy of
# the same hang, so a stopped task with a task after it is proven under the
# running dispatcher without spending the one lane the case below still needs.
echo hang > "$CTL/paused-route"
task_doc "$LIVE/paused-route.md" paused-route "$BODY" "group: paused-route"
must "a second task, to prove the route" \
  "$SPOOLWAY" queue add --from "$LIVE/paused-route.md"
task_doc "$LIVE/paused-dep.md" paused-dep "$BODY" "group: paused-route" \
  "depends_on: [paused-route]"
must "and a task that waits on it" "$SPOOLWAY" queue add --from "$LIVE/paused-dep.md"
if drive paused-route implement 120; then ok "it reaches implement too"
else bad "it reaches implement too (at \`$(stage_of paused-route)\`)"; fi
must "the route the refusal names first actually works" \
  "$SPOOLWAY" queue pause paused-route
if drive paused-route paused 60; then ok "and the task really does stop, checkout kept"
else bad "and the task really does stop, checkout kept (at \`$(stage_of paused-route)\`)"; fi
works "with its checkout still standing" \
  test -n "$(worktree_of paused-route)"

# A stopped task is torn down although the dispatcher still runs: nothing is
# moving it. Its worktree and branch are deleted, whatever commits the branch
# holds, with no commit made first, and it lands in pending ready to queue
# again with no leftover branch to refuse it. The task after it goes with it.
PAUSED_WORKTREE=$(worktree_of paused-route)
must "the stopped task's branch holds a commit no remote has" \
  git -C "$PAUSED_WORKTREE" commit -q --allow-empty -m "work no remote has"
PAUSED_OUT="$LIVE/unqueue-paused-route.out"
if "$SPOOLWAY" queue unqueue paused-route >"$PAUSED_OUT" 2>&1; then
  ok "unqueue takes a stopped task while a dispatcher runs"
else
  bad "unqueue takes a stopped task while a dispatcher runs"
  sed 's/^/        /' "$PAUSED_OUT"
fi
has "it says what it deleted" \
  "deleted branch task/paused-route and its worktree" "$PAUSED_OUT"
has "it says it unqueued" "unqueued \`paused-route\`" "$PAUSED_OUT"
has "and the task after it" "unqueued \`paused-dep\`" "$PAUSED_OUT"
works "the task after it is in pending too" \
  test -f "$SPOOLWAY_PROJECT_HOME/pending/paused-dep.md"
lacks "and names no kept branch" "kept branch" "$PAUSED_OUT"
works "the queue file is gone" test ! -e "$SPOOLWAY_PROJECT_HOME/queue/paused-route.md"
works "the task reached pending" test -f "$SPOOLWAY_PROJECT_HOME/pending/paused-route.md"
if [ -n "$PAUSED_WORKTREE" ] && [ ! -d "$PAUSED_WORKTREE" ]; then
  ok "its worktree is gone"
else
  bad "its worktree is gone (\"$PAUSED_WORKTREE\" still exists)"
fi
works "its branch is deleted" test -z "$(git branch --list task/paused-route)"
git log --all --format=%s >"$LIVE/paused-route.log"
lacks "no wip commit was made" "wip(paused-route)" "$LIVE/paused-route.log"
must "the carried task passes the task contract" \
  "$SPOOLWAY" task contract --from "$SPOOLWAY_PROJECT_HOME/pending/paused-route.md"
must "and queues again with no leftover branch to refuse it" \
  "$SPOOLWAY" queue add --from "$SPOOLWAY_PROJECT_HOME/pending/paused-route.md"
# Not left queued: the running dispatcher would start it again on a fresh lane.
must "and is unqueued again before the dispatcher can start it" \
  "$SPOOLWAY" queue unqueue paused-route
works "the dependent that went to pending is not in the queue" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/queue/paused-dep.md"

# The dispatcher stops, and a running `solo` is still refused: a lane at work
# in the checkout is what the teardown must not race, dispatcher or not.
dispatcher_stop
NODISP_OUT="$LIVE/unqueue-refused-nodispatcher.out"
if "$SPOOLWAY" queue unqueue solo >"$NODISP_OUT" 2>&1; then
  bad "a running task is refused with no dispatcher too"
else
  ok "a running task is refused with no dispatcher too"
fi
has "naming the pause command again" "spoolway queue pause solo" "$NODISP_OUT"
if kill -0 "$LANE_PID" 2>/dev/null; then ok "and the lane is still running"
else bad "and the lane is still running"; fi

# Pausing it, as the refusal says, stops the lane; the unqueue after it tears
# down and unqueues in one command, and nothing is left of the checkout on the
# document that reaches pending.
must "pausing it stops the lane" "$SPOOLWAY" queue pause solo
solo_paused() { [ "$(stage_of solo)" = paused ]; }
if poll_until 30 solo_paused; then ok "and the task parks on paused"
else bad "and the task parks on paused (at \`$(stage_of solo)\`)"; fi
FORCE_OUT="$LIVE/unqueue-solo.out"
if "$SPOOLWAY" queue unqueue solo >"$FORCE_OUT" 2>&1; then
  ok "unqueue tears down the paused task and unqueues it"
else
  bad "unqueue tears down the paused task and unqueues it"
  sed 's/^/        /' "$FORCE_OUT"
fi
has "it says what it deleted" "deleted branch task/solo and its worktree" "$FORCE_OUT"
has "and that it unqueued" "unqueued \`solo\`" "$FORCE_OUT"
works "its branch is deleted" test -z "$(git branch --list task/solo)"
works "the queue file is gone" test ! -e "$SPOOLWAY_PROJECT_HOME/queue/solo.md"
works "the task reached pending" test -f "$SPOOLWAY_PROJECT_HOME/pending/solo.md"
lacks "with no checkout field left on it: worktree_path" \
  "worktree_path:" "$SPOOLWAY_PROJECT_HOME/pending/solo.md"
lacks "workspace_id" "workspace_id:" "$SPOOLWAY_PROJECT_HOME/pending/solo.md"
lacks "pane_id" "pane_id:" "$SPOOLWAY_PROJECT_HOME/pending/solo.md"
lacks "tab_id" "tab_id:" "$SPOOLWAY_PROJECT_HOME/pending/solo.md"
if [ -n "$SOLO_WORKTREE" ] && [ ! -d "$SOLO_WORKTREE" ]; then
  ok "and the worktree itself is gone"
else
  bad "and the worktree itself is gone (\"$SOLO_WORKTREE\" still exists)"
fi
if poll_while 15 kill -0 "$LANE_PID"; then
  ok "and the lane it interrupted is really over"
else
  bad "and the lane it interrupted is really over"
fi

# Run again over the same task, as a second person or the board would after the
# first run finished: the task is no longer in the queue, so the command
# refuses cleanly and the pending document the first run wrote stays as it was.
BEFORE_AGAIN=$(cksum <"$SPOOLWAY_PROJECT_HOME/pending/solo.md")
if "$SPOOLWAY" queue unqueue solo >"$LIVE/unqueue-again.out" 2>&1; then
  bad "a second unqueue over the same task refuses"
else
  ok "a second unqueue over the same task refuses"
fi
works "and leaves the pending document untouched" \
  test "$BEFORE_AGAIN" = "$(cksum <"$SPOOLWAY_PROJECT_HOME/pending/solo.md")"
works "and its branch stays deleted" test -z "$(git branch --list task/solo)"

# ------------------------------------- a started task whose branch was never made
# `queue add` stamps `branch: task/<id>` before anything is cut, so a task
# can name a branch that does not exist. Deleting the real one afterwards
# leaves the same shape on disk. Only a repo with a remote asks
# whether a branch is pushed at all, so one is added here. The unqueue must then
# say nothing about a kept branch: there is no branch to keep, rename or
# delete, and the line would send a person after one.
echo hang > "$CTL/ghost"
task_doc "$LIVE/ghost.md" ghost "$BODY" "group: ghost"
must "a task whose branch will be deleted" "$SPOOLWAY" queue add --from "$LIVE/ghost.md"
if drive_and_hold ghost implement 120; then ok "it reaches implement, then the dispatcher stops"
else bad "it reaches implement, then the dispatcher stops (at \`$(stage_of ghost)\`)"; fi
GHOST_WORKTREE=$(worktree_of ghost)
must "a remote, so the push state is asked" \
  git init -q --bare -b main "$LIVE/ghost-origin.git"
must "wired up as origin" git remote add origin "$LIVE/ghost-origin.git"
must "its checkout is removed from under it" \
  git worktree remove --force "$GHOST_WORKTREE"
must "and its branch deleted, as if it had never been cut" \
  git branch -D task/ghost
works "so the branch it names does not exist" \
  test -z "$(git branch --list task/ghost)"
must "pausing it, as an unqueue of a running task says" "$SPOOLWAY" queue pause ghost
GHOST_OUT="$LIVE/unqueue-ghost.out"
if "$SPOOLWAY" queue unqueue ghost >"$GHOST_OUT" 2>&1; then
  ok "unqueue takes it"
else
  bad "unqueue takes it"
  sed 's/^/        /' "$GHOST_OUT"
fi
has "it says it unqueued" "unqueued \`ghost\`" "$GHOST_OUT"
lacks "and prints no kept-branch line" "kept branch" "$GHOST_OUT"
lacks "and no deleted-branch line, since there was no branch" "deleted branch" "$GHOST_OUT"
works "the task reached pending" test -f "$SPOOLWAY_PROJECT_HOME/pending/ghost.md"
if [ -n "$GHOST_WORKTREE" ] && [ ! -d "$GHOST_WORKTREE" ]; then
  ok "and its worktree is gone"
else
  bad "and its worktree is gone (\"$GHOST_WORKTREE\" still exists)"
fi

# ----------------------------------------------- a started copy of a routine
# The routine is still under `.spoolway/routines/` to queue again, so
# unqueue tears the copy's checkout down and deletes the copy rather than
# drafting a duplicate of the routine into pending. Only the routines tab mints
# a copy, so a queued task is given the `routine:` line it would have carried,
# once its dispatcher has stopped. A commit is made on its branch first: the
# teardown deletes the branch although it holds more than any remote has.
echo hang > "$CTL/rt-1"
mkdir -p .spoolway/routines/rt
task_doc .spoolway/routines/rt/rt.md rt "$BODY" "group: rt"
task_doc "$LIVE/rt-1.md" rt-1 "$BODY" "group: rt"
must "a task that will be a routine's copy" "$SPOOLWAY" queue add --from "$LIVE/rt-1.md"
if drive_and_hold rt-1 implement 120; then ok "it reaches implement, then the dispatcher stops"
else bad "it reaches implement, then the dispatcher stops (at \`$(stage_of rt-1)\`)"; fi
RT_WORKTREE=$(worktree_of rt-1)
must "its branch holds a commit no remote has" \
  git -C "$RT_WORKTREE" commit -q --allow-empty -m "work no remote has"
must "it is stamped as a copy of the routine" \
  sed -i 's/^group: rt$/group: rt\nroutine: rt/' "$SPOOLWAY_PROJECT_HOME/queue/rt-1.md"
must "pausing it, as an unqueue of a running task says" "$SPOOLWAY" queue pause rt-1
RT_OUT="$LIVE/unqueue-routine-copy.out"
if "$SPOOLWAY" queue unqueue rt-1 >"$RT_OUT" 2>&1; then
  ok "unqueue takes the started copy"
else
  bad "unqueue takes the started copy"
  sed 's/^/        /' "$RT_OUT"
fi
has "it says it deleted the copy's branch and worktree" \
  "deleted branch task/rt-1 and its worktree" "$RT_OUT"
has "it says the copy was deleted" "deleted \`rt-1\`" "$RT_OUT"
has "naming the routine file" ".spoolway/routines/rt/rt.md" "$RT_OUT"
lacks "and names no kept branch" "kept branch" "$RT_OUT"
works "its branch is deleted" test -z "$(git branch --list task/rt-1)"
works "the queue file is gone" test ! -e "$SPOOLWAY_PROJECT_HOME/queue/rt-1.md"
works "nothing was written to pending" test ! -e "$SPOOLWAY_PROJECT_HOME/pending/rt-1.md"
works "and the routine file is untouched" test -f .spoolway/routines/rt/rt.md
if [ -n "$RT_WORKTREE" ] && [ ! -d "$RT_WORKTREE" ]; then
  ok "and its worktree is gone"
else
  bad "and its worktree is gone (\"$RT_WORKTREE\" still exists)"
fi

# ------------------------------------------ closing a stuck task lets its dependent go on
# `resume --stage done` is the way out for a task that cannot pass: it moves a
# paused or blocked task to `done`, and the dispatcher then archives it and
# starts whatever waited on it. A unit test settles the file edits; only a real
# dispatcher pass and a real dependent show that the dependent actually starts.
# A task still running on a step is refused, so it is paused first.
echo hang > "$CTL/closer"
echo hang > "$CTL/after-closer"
task_doc "$LIVE/closer.md" closer "$BODY" "group: closer"
must "a task whose lane will hang" "$SPOOLWAY" queue add --from "$LIVE/closer.md"
task_doc "$LIVE/after-closer.md" after-closer "$BODY" "group: closer" \
  "depends_on: [closer]"
must "and a task that waits on it" "$SPOOLWAY" queue add --from "$LIVE/after-closer.md"
if drive closer implement 120; then ok "the first task reaches implement and hangs"
else bad "the first task reaches implement and hangs (at \`$(stage_of closer)\`)"; fi
CLOSER_PID=$(lane_pid "closer · implement" 30)
if [ -n "$CLOSER_PID" ]; then ok "its lane really is mid-turn"
else bad "its lane really is mid-turn"; fi
lacks "the dependent has not started" "worktree_path:" \
  "$SPOOLWAY_PROJECT_HOME/queue/after-closer.md"
refuses "a task still running on a step is not closed" "queue pause closer" \
  "$SPOOLWAY" resume closer --stage done
must "pausing it stops the lane" "$SPOOLWAY" queue pause closer
if drive closer paused 60; then ok "and the task parks on paused"
else bad "and the task parks on paused (at \`$(stage_of closer)\`)"; fi

CLOSE_OUT="$LIVE/close.out"
# The close is refused for the moment while the dispatcher is still booting a
# lane for the task, and says to run it again; a pass that read the task just
# before the pause can be doing exactly that. So it is asked again until it
# takes.
close_closer() {
  "$SPOOLWAY" resume closer --stage done -m "work already on main" >"$CLOSE_OUT" 2>&1
}
if poll_until 30 close_closer; then
  ok "\`resume --stage done\` closes a task standing on a step"
else
  bad "\`resume --stage done\` closes a task standing on a step"
  sed 's/^/        /' "$CLOSE_OUT"
fi
has "it says what it skipped" "closer: -> done (skipped: " "$CLOSE_OUT"
if drive closer gone 60; then ok "the dispatcher archives the closed task"
else bad "the dispatcher archives the closed task (at \`$(stage_of closer)\`)"; fi
if wait_for_text 60 "$E2E_DISPATCH_LOG" "started after-closer · implement"; then
  ok "and the task that waited on it starts"
else
  bad "and the task that waited on it starts"
  tail -20 "$E2E_DISPATCH_LOG" | sed 's/^/        /'
fi
dispatcher_stop

# ---------------------------------------------------- the removed flags
# `--force` and `--all` are gone, not flags that do nothing: clap's own
# unknown-argument error.
refuses "\`--force\` is an unknown argument" "unexpected argument '--force'" \
  "$SPOOLWAY" queue unqueue base --force
refuses "\`--all\` is an unknown argument" "unexpected argument '--all'" \
  "$SPOOLWAY" queue unqueue --all

# ------------------------------------------------- a hook-failed task, requeued
# A failing `queued` hook moves the task to `hook_failed` and leaves
# `hook_paused:`, `hook_error:` and the hook's exit file behind. Unqueued and queued again, the task has to start
# over: no pause record in the pending copy, no exit file under `tracking/`,
# and both `queued` and `started` hooks fired anew rather than the old exit
# code being read as the new run's answer.
HOOKLOG="$LIVE/requeue-hook.log"
HOOK_OK="$LIVE/requeue-hook.ok"
TRACKING="$SPOOLWAY_PROJECT_HOME/tracking"
mkdir -p "$LIVE/proj/.spoolway/hooks"
cat > "$LIVE/proj/.spoolway/hooks/requeue.sh" <<HOOK
#!/bin/sh
[ "\$SPOOLWAY_EVENT" = open ] && exit 0
echo "\$SPOOLWAY_EVENT \$SPOOLWAY_TASK" >> "$HOOKLOG"
[ -f "$HOOK_OK" ] && exit 0
exit 1
HOOK
chmod +x "$LIVE/proj/.spoolway/hooks/requeue.sh"
must "a hook that fails until told otherwise" \
  "$SPOOLWAY" config set issue_tracking.hook requeue.sh

echo hang > "$CTL/hook-requeue"
task_doc "$LIVE/hook-requeue.md" hook-requeue "$BODY" "group: hook-requeue" \
  "group_description: a task under a hook that fails until told otherwise"
must "a task queues under it" "$SPOOLWAY" queue add --from "$LIVE/hook-requeue.md"
if drive hook-requeue hook_failed 60; then ok "the failing queued hook stops it on hook_failed"
else bad "the failing queued hook stops it on hook_failed (at \`$(stage_of hook-requeue)\`)"; fi
has "with the stop recorded as a hook's" "hook_paused: queued" \
  "$SPOOLWAY_PROJECT_HOME/queue/hook-requeue.md"
works "and the hook's exit file on disk" \
  test -f "$TRACKING/hook-requeue · queued.exit"
dispatcher_stop

must "a hook-failed task unqueues" \
  "$SPOOLWAY" queue unqueue hook-requeue
lacks "the pending copy carries no hook_paused" "hook_paused:" \
  "$SPOOLWAY_PROJECT_HOME/pending/hook-requeue.md"
lacks "and no hook_error" "hook_error:" \
  "$SPOOLWAY_PROJECT_HOME/pending/hook-requeue.md"
lacks "and no missing_start_branch" "missing_start_branch:" \
  "$SPOOLWAY_PROJECT_HOME/pending/hook-requeue.md"
works "and no hook run file of the old run is left" \
  test -z "$(find "$TRACKING" -name 'hook-requeue · *' 2>/dev/null)"

touch "$HOOK_OK"
: > "$HOOKLOG"
must "it queues again" "$SPOOLWAY" queue add --from \
  "$SPOOLWAY_PROJECT_HOME/pending/hook-requeue.md"
if drive hook-requeue implement 60; then ok "and now gets past the hook to implement"
else bad "and now gets past the hook to implement (at \`$(stage_of hook-requeue)\`)"; fi
has "the queued hook fired again" "queued hook-requeue" "$HOOKLOG"
has "and so did the started hook" "started hook-requeue" "$HOOKLOG"
dispatcher_stop

# ------------------------------------------------ the tool-requirements gate
# The one route the mockup itself draws: the queue screen's own `enter`,
# gated over a hook whose declared `gh` floor this machine's `gh` cannot
# meet — a `--version`-only double is enough, since the hook this gate
# exists to keep uncalled is never asked for anything else. `esc` returns to
# the queue screen with nothing queued and nothing opened; `enter` over the
# gate queues the batch anyway, with `open_tickets` never reached at all —
# no `ticket:`, no `epic:`, on the task that lands in the queue. `init` ran
# with no tracker and wrote no hooks, so the shipped `github.sh` goes on disk
# first.
must "the shipped github.sh is written" \
  "$SPOOLWAY" sync --replace .spoolway/hooks/github.sh
must "the hook is set to github.sh" \
  "$SPOOLWAY" config set issue_tracking.hook github.sh
must "and a project key" \
  "$SPOOLWAY" config set issue_tracking.project_key acme/app

LOWVER_BIN="$LIVE/gh-lowver-bin"
mkdir -p "$LOWVER_BIN"
cat >"$LOWVER_BIN/gh" <<'GHSTUB'
#!/bin/sh
case "$1" in
  --version) echo "gh version 2.46.0 (2024-01-01)"; exit 0 ;;
esac
exit 1
GHSTUB
chmod +x "$LOWVER_BIN/gh"

pending_doc gate-esc "$BODY" "group: gate-esc"

GATE_ESC_OUT="$LIVE/gate-esc.out"
on_screen ' \r\x1b' "$GATE_ESC_OUT" PATH="$LOWVER_BIN:$PATH"
has "the gate draws over an unmet gh version" \
  "issue tracking is not supported." "$GATE_ESC_OUT"
has "naming the declared floor" "gh >= 2.97.0" "$GATE_ESC_OUT"
has "in a popup over the queue screen" "╭─ issue tracking " "$GATE_ESC_OUT"
has "whose body says what enter does" \
  "enter queues it without issue tracking." "$GATE_ESC_OUT"
lacks "with no answer row" \
  "> queue anyway, without issue tracking" "$GATE_ESC_OUT"
has "under the shared key row" "[enter] confirm   [esc] close" "$GATE_ESC_OUT"
works "esc leaves the task in pending, unqueued" \
  test -f "$SPOOLWAY_PROJECT_HOME/pending/gate-esc.md"
works "and nothing reached the queue" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/queue/gate-esc.md"

GATE_ENTER_OUT="$LIVE/gate-enter.out"
on_screen ' \r\rn' "$GATE_ENTER_OUT" PATH="$LOWVER_BIN:$PATH"
has "the gate draws the same way when enter is pressed instead" \
  "issue tracking is not supported." "$GATE_ENTER_OUT"
works "enter over the gate queues the batch anyway" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/gate-esc.md"
lacks "with the hook never invoked — no ticket written" \
  "ticket:" "$SPOOLWAY_PROJECT_HOME/queue/gate-esc.md"
lacks "and no epic either" "epic:" "$SPOOLWAY_PROJECT_HOME/queue/gate-esc.md"

# The same gate, reached the other way it is reached: `queue add --from`
# itself, run over a real pty rather than the queue screen's own `on_screen`
# fixed key script above. `write_pty_driver` (`lib.sh`) gives the process a
# real terminal without needing one behind this suite's own process — the
# same way `commands.sh` proves `spoolway sync`'s panel (gh-528). The driver
# waits for the gate's own text to land before sending `esc`, since a key
# sent before the guard takes raw mode still waits in the tty's buffer
# until the read.
PTY_DRIVER="$LIVE/gate-from-pty.py"
write_pty_driver "$PTY_DRIVER"

pending_doc gate-from "$BODY" "group: gate-from"
GATE_FROM_OUT="$LIVE/gate-from.out"
PATH="$LOWVER_BIN:$PATH" python3 "$PTY_DRIVER" \
  --after "issue tracking is not supported." --keys $'\033' \
  "$SPOOLWAY" queue add --from "$SPOOLWAY_PROJECT_HOME/pending/gate-from.md" \
  >"$GATE_FROM_OUT" 2>&1
has "the gate's popup reaches the terminal over \`--from\` too" \
  "> queue anyway, without issue tracking" "$GATE_FROM_OUT"
has "under the same shared key row" "[enter] confirm   [esc] close" "$GATE_FROM_OUT"
lacks "and \`--from\` never enters the alternate screen either" \
  $'\033[?1049h' "$GATE_FROM_OUT"
works "esc leaves that task in pending too, unqueued" \
  test -f "$SPOOLWAY_PROJECT_HOME/pending/gate-from.md"

finish
