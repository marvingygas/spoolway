#!/usr/bin/env bash
# `spoolway queue unqueue`, the command form of the board's `u`/`U` keys.
#
# The move itself — carrying a document back to pending with every reserved
# key stripped — is `status::unqueue_task`, already proven against the board
# by `board-pause.sh`'s own `U` case. What is only true of the command, and
# needs a real process to prove, is everything a script gets that a keypress
# does not: `--help` actually listing the word, `queue remove` actually
# getting nothing but clap's own unknown-subcommand error, a refusal that
# names two routes which both actually work, and `--force` actually
# interrupting a real lane, committing real uncommitted work and tearing a
# real worktree down before the move — not a description of any of that, the
# thing itself.
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
configure_project plan/unqueue "$LIVE/worktrees"

BODY="$LIVE/body.md"
task_body "$BODY"

# ------------------------------------------------------- the round trip out and back
# A dependency still queued refuses a bare unqueue of the task it depends on
# — this command has no panel to list a chain on, unlike the board's own `u`,
# which now carries that dependent back to pending alongside it instead.
# `--all` carries both back together, with no per-task check, exactly as `U`
# does.
task_doc "$LIVE/base.md" base "$BODY" "group: unq" "touches: [notes/base.md]"
must "base queues" "$SPOOLWAY" queue add --from "$LIVE/base.md"
task_doc "$LIVE/dependent.md" dependent "$BODY" "group: unq" \
  "touches: [notes/dependent.md]" "depends_on: [base]"
must "a sibling that depends on it queues too" "$SPOOLWAY" queue add --from "$LIVE/dependent.md"

refuses "unqueuing base alone, with dependent still queued behind it" "dependent" \
  "$SPOOLWAY" queue unqueue base
works "base is still queued — the refusal changed nothing" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/base.md"

must "--all carries every not-started task back at once" \
  "$SPOOLWAY" queue unqueue --all
works "base landed in pending" test -f "$SPOOLWAY_PROJECT_HOME/pending/base.md"
works "dependent landed in pending with it" \
  test -f "$SPOOLWAY_PROJECT_HOME/pending/dependent.md"
works "neither is in the queue any more" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/queue/base.md"

must "the round-tripped document still passes the task contract" \
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
# A real lane, hung mid-turn, so the checkout the refusal names is a real one
# and the teardown `--force` runs is a real teardown, not a description of it.
echo hang > "$CTL/solo"
task_doc "$LIVE/solo.md" solo "$BODY" "group: unq" "touches: [notes/solo.md]"
must "a task whose lane will hang mid-turn" "$SPOOLWAY" queue add --from "$LIVE/solo.md"

if drive solo implement 60; then ok "it reaches implement and sits there"
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
# genuinely hung and never reports, so the refusal and the forced teardown
# below race nothing.
UNQUEUE_OUT="$LIVE/unqueue-refused.out"
if "$SPOOLWAY" queue unqueue solo >"$UNQUEUE_OUT" 2>&1; then
  bad "an unqueue with no --force is refused"
  sed 's/^/        /' "$UNQUEUE_OUT"
else
  ok "an unqueue with no --force is refused"
fi
has "naming the stage it is actually on" "is on \`implement\`, not \`queued\`" "$UNQUEUE_OUT"
has "the checkout holding it back" "$SOLO_WORKTREE" "$UNQUEUE_OUT"
has "the route that stops it where it stands" "spoolway queue pause solo" "$UNQUEUE_OUT"
has "and the route that tears it down" "spoolway queue unqueue solo --force" "$UNQUEUE_OUT"
has "and says plainly that nothing moved" "Nothing was changed." "$UNQUEUE_OUT"
works "the refusal changed nothing: still queued" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/solo.md"
if kill -0 "$LANE_PID" 2>/dev/null; then ok "and the lane is still running"
else bad "and the lane is still running"; fi

# Both routes the refusal named actually work. `pause` first, over a copy of
# the same hang, so the refusal's other route is proven too without spending
# the one lane the forced case below still needs.
echo hang > "$CTL/paused-route"
task_doc "$LIVE/paused-route.md" paused-route "$BODY" "group: unq" \
  "touches: [notes/paused-route.md]"
must "a second task, to prove the other route" \
  "$SPOOLWAY" queue add --from "$LIVE/paused-route.md"
if drive paused-route implement 60; then ok "it reaches implement too"
else bad "it reaches implement too (at \`$(stage_of paused-route)\`)"; fi
must "the route the refusal names first actually works" \
  "$SPOOLWAY" queue pause paused-route
if drive paused-route paused 30; then ok "and the task really does stop, checkout kept"
else bad "and the task really does stop, checkout kept (at \`$(stage_of paused-route)\`)"; fi
works "with its checkout still standing" \
  test -n "$(worktree_of paused-route)"

# `--force` refuses outright while the suite's own dispatcher holds the run
# lock, naming its pid — it re-reads the queue every pass, and a checkout
# this tears down out from under it is exactly the corruption the lock
# exists to prevent.
LOCK_OUT="$LIVE/unqueue-force-locked.out"
if "$SPOOLWAY" queue unqueue solo --force >"$LOCK_OUT" 2>&1; then
  bad "\`--force\` refuses while a dispatcher holds the lock"
  sed 's/^/        /' "$LOCK_OUT"
else
  ok "\`--force\` refuses while a dispatcher holds the lock"
fi
has "naming the pid holding it" "already running for this repo (pid " "$LOCK_OUT"
works "solo is untouched: still queued, lane still running" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/solo.md"
if kill -0 "$LANE_PID" 2>/dev/null; then ok "and the lane is still running"
else bad "and the lane is still running"; fi

# Stopped, so the forced route below is not refused by the very lock it just
# proved — the same lock a person would `spoolway dispatch` past by stopping
# their own run first.
dispatcher_stop

# The forced route, over `solo`: interrupt, commit, tear down, unqueue — all
# in one command, and nothing left of the checkout on the document that
# reaches pending.
FORCE_OUT="$LIVE/unqueue-forced.out"
if "$SPOOLWAY" queue unqueue solo --force >"$FORCE_OUT" 2>&1; then
  ok "\`--force\` interrupts, commits, tears down and unqueues"
else
  bad "\`--force\` interrupts, commits, tears down and unqueues"
  sed 's/^/        /' "$FORCE_OUT"
fi
has "it says the lane was interrupted" "interrupted lane implement/solo" "$FORCE_OUT"
has "and that it unqueued" "unqueued \`solo\`" "$FORCE_OUT"
works "the queue file is gone" test ! -e "$SPOOLWAY_PROJECT_HOME/queue/solo.md"
works "the document reached pending" test -f "$SPOOLWAY_PROJECT_HOME/pending/solo.md"
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

# ------------------------------------------------------------- --all --force
# Refused outright, in either flag order — tearing down every checkout in the
# queue is not a command a script should reach by accident.
refuses "\`--all --force\` is refused" "--all --force" \
  "$SPOOLWAY" queue unqueue --all --force
refuses "in the other flag order too" "--all --force" \
  "$SPOOLWAY" queue unqueue --force --all
works "paused-route's checkout is untouched by either refusal" \
  test -n "$(worktree_of paused-route)"

finish
