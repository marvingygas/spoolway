#!/usr/bin/env bash
# The ways a run ends badly, held against a real dispatcher and real detached
# lanes — a hard kill with lanes live, the stale lock it leaves, a restart
# over a still-running lane, a lane that reports with nobody listening, a
# stop that sweeps and one that does not, and a multiplexer that dies under
# worktrees that outlive it.
#
# `dispatch.backend = headless` makes every lane a real `setsid` process of
# its own — see `fixture.sh` — which is what makes a kill of the
# *dispatcher's* group the disaster scenario exactly: the lane is in a
# session of its own by construction, and survives untouched. So most of
# this suite kills the dispatcher directly, by pid, rather than through
# `lib.sh`'s `dispatcher_start`/`dispatcher_stop` pair — those exist to keep
# one dispatcher running across a suite's whole life, and every case here
# wants to end one on purpose and look at what it left behind.
#
# A mid-turn lane, everywhere one is needed, is the `hang` mode `agents/pi`
# already has — it sleeps for five minutes and never reports on its own —
# reached either by naming a task `hang` directly or, where a case wants a
# name of its own, by seeding `$CTL/<task>` with `hang` before it queues: the
# stand-in reads that file before it reads its own task id. The tmux case is
# the one exception, and says why where it queues its own task.
#
# covers: dispatch.tear_lanes_on_stop — a stop with the teardown on ends the lane before it takes the worktree, and keeps the branch
# covers: dispatch.tear_lanes_on_stop — a stop with the teardown off leaves every worktree and lane exactly where it stood
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
configure_project plan/disaster "$LIVE/worktrees"

BODY="$LIVE/body.md"
task_body "$BODY"

# queue_hang <task-id>
#
# A task whose implement lane never returns on its own: `agents/pi` sleeps on
# `hang` or `vanish` as its own task id, and on anything a ctl file names —
# see the stand-in's own comment on which one wins. Named tasks other than
# `hang` itself need the file; `hang` needs nothing; both go through this one
# helper so every case reads the same.
queue_hang() {
  local id=$1
  mkdir -p "$CTL"
  [ "$id" = hang ] || echo hang > "$CTL/$id"
  task_doc "$LIVE/$id.md" "$id" "$BODY" "group: disaster" "touches: [notes/$id.md]"
  must "$id queues" "$SPOOLWAY" queue add --from "$LIVE/$id.md"
}

# Kill the tracked dispatcher's whole process group with `sig`, and wait for
# every member of it to be gone before handing control back — a case that
# asserted the very next instant would be racing the signal's own delivery
# rather than testing anything about spoolway.
kill_dispatcher() {
  local sig=$1
  [ -n "${E2E_DISPATCHER_PID:-}" ] || return 0
  kill "-$sig" -- "-$E2E_DISPATCHER_PID" 2>/dev/null
  poll_while 10 kill -0 -- "-$E2E_DISPATCHER_PID"
  E2E_DISPATCHER_PID=""
}

# The other way a run ends: `ctrl-c`, caught by
# `crate::platform::stop::catch_interrupt` and answered by `dispatch::stop` —
# the sweep several of these cases are about. `SIGTERM`, which
# `kill_dispatcher` also offers, is never caught and never runs that path.
#
# `SIGINT` is not an option against `lib.sh`'s own resident dispatcher,
# though: bash ignores `SIGINT` of its own accord while it is waiting on a
# foreground child — the standard shell answer to `ctrl-c`, so that the shell
# survives a child that catches the signal and keeps going — and
# `dispatcher_start`'s supervisor is exactly that shape, a `while` loop
# waiting on `"$spoolway" dispatch`. `spoolway` catches the signal, sweeps,
# and exits *cleanly* — which the loop reads as "round again" and relaunches
# within half a second, so a case watching for the process to be gone would
# be watching a dispatcher that came straight back, under a different pid,
# and picked up whatever else was queued. Found by watching one do exactly
# that: `stopping: gave back 1 worktree(s)` immediately followed by another
# `started`, in a suite run that kept every lane it was meant to end.
#
# One turn on its own, `exec`'d rather than looped, is what makes `SIGINT`
# final: the pid a case signals is the pid of `spoolway` itself, with no
# shell left standing over it to call it again.
one_shot_start() {
  local pidfile="$LIVE/one-shot.pid"
  rm -f "$pidfile"
  setsid bash -c 'echo $$ >"$1"; shift; exec "$@"' \
    _ "$pidfile" "$SPOOLWAY" dispatch --plain --interval "${E2E_INTERVAL:-1s}" \
    >>"$E2E_DISPATCH_LOG" 2>&1 &
  poll_until 10 test -s "$pidfile" || {
    printf '  \033[31mSETUP\033[0m the one-shot dispatcher never started\n' >&2
    exit 2
  }
  cat "$pidfile"
}

# `one_shot_start`'s own counterpart with a board drawn instead of `--plain`
# — the one thing this suite needs it for is proving a pass's problems reach
# `problem_log` and never this process's own output. Stdin closed rather than
# inherited: a board takes the terminal for as long as it is up, and this one
# has to find none to take. `TermGuard` already no-ops off a stdin that is not
# a terminal, so `/dev/null` here is what keeps a backgrounded board from
# reaching for a real one.
one_shot_start_board() {
  local pidfile="$LIVE/one-shot-board.pid"
  rm -f "$pidfile"
  setsid bash -c 'echo $$ >"$1"; shift; exec "$@"' \
    _ "$pidfile" "$SPOOLWAY" dispatch --interval "${E2E_INTERVAL:-1s}" \
    >>"$E2E_DISPATCH_LOG" 2>&1 </dev/null &
  poll_until 10 test -s "$pidfile" || {
    printf '  \033[31mSETUP\033[0m the one-shot board dispatcher never started\n' >&2
    exit 2
  }
  cat "$pidfile"
}

# `ctrl-c`, and wait for that one process — no group needed, since
# `one_shot_start` never leaves a shell above it to also be a member of one.
one_shot_stop() {
  local pid=$1
  [ -n "$pid" ] || return 0
  kill -INT "$pid" 2>/dev/null
  poll_while 15 kill -0 "$pid"
}

# Sweep whatever a case left running, with the teardown on, so one case's
# mess is never what the next one reads. Not a check itself — nothing here
# calls `ok` or `bad` — so it costs nothing on the tally either way.
sweep() {
  must "the teardown is on, to clear whatever this case left running" \
    "$SPOOLWAY" config set dispatch.tear_lanes_on_stop true
  # Any resident supervisor first, hard — a one-shot pass racing a second
  # dispatcher for the lock would only ever watch it, never sweep anything.
  kill_dispatcher KILL
  one_shot_stop "$(one_shot_start)"
}

# A swept task is not a *done* one — the sweep gives back its lane and its
# worktree and leaves it exactly on the step it stood on, ready for a future
# pass to pick up right where it stopped, which is the whole point of a stop
# being resumable. This suite's own `hang`-mode tasks never report, so
# nothing ever moves them on; left in the queue, every pass from here on
# would restart one for no reason a later case is asking about — and under
# the tmux case, on the wrong backend entirely, since a headless task's
# recorded workspace answers to nothing a tmux `workspace_alive` check would
# recognise. Deleting the task document is what a person abandoning a task by
# hand would do too — there is no `queue remove`, because there is ordinarily
# no reason to want one.
forget() {
  rm -f "$SPOOLWAY_PROJECT_HOME/queue/$1.md"
}

# ------------------------------------------------------- a hard kill, live
dispatcher_start
queue_hang hang
PID1=$(lane_pid "hang · implement" 20)
WT1=$(worktree_of hang)
if [ -n "$PID1" ] && [ -n "$WT1" ]; then
  kill_dispatcher KILL
  if kill -0 "$PID1" 2>/dev/null \
     && [ "$(stage_of hang)" = implement ] \
     && [ -d "$WT1" ]; then
    ok "a kill leaves every lane running and the queue where it stood"
  else
    bad "a kill leaves every lane running and the queue where it stood"
    printf '        lane pid %s alive: %s, stage: %s, worktree: %s\n' \
      "$PID1" "$(kill -0 "$PID1" 2>/dev/null && echo yes || echo no)" \
      "$(stage_of hang)" "$([ -d "$WT1" ] && echo present || echo gone)"
  fi
else
  bad "a kill leaves every lane running and the queue where it stood (the lane never started)"
fi

# --------------------------------------------------- the lock it left behind
LOCKFILE="$SPOOLWAY_PROJECT_HOME/dispatch.pid"
OLD_LOCK_PID=$(head -1 "$LOCKFILE" 2>/dev/null || true)
new_holder() {
  local p
  p=$(head -1 "$LOCKFILE" 2>/dev/null) || return 1
  [ -n "$p" ] && [ "$p" != "$OLD_LOCK_PID" ] && kill -0 "$p" 2>/dev/null
}
# Never deleted by this suite — `Lock::acquire` is what has to see through a
# dead pid on its own, or this is testing a suite's own housekeeping instead.
dispatcher_start
if [ -n "$OLD_LOCK_PID" ] && ! kill -0 "$OLD_LOCK_PID" 2>/dev/null \
   && poll_until 10 new_holder \
   && ! grep -q 'watching dispatcher' "$E2E_DISPATCH_LOG"; then
  ok "the lock file a killed dispatcher left behind is not a holder"
else
  bad "the lock file a killed dispatcher left behind is not a holder"
  printf '        old holder: %s, lock now: %s\n' "$OLD_LOCK_PID" "$(cat "$LOCKFILE" 2>/dev/null)"
fi

# ------------------------------------------------ a restart, over a live lane
# The dispatcher just (re)started above is running now, with `hang`'s lane
# from the very first case still alive and unreported. Its own reconciliation
# — matching a task's step against `Mux::list_lanes` — is what has to notice
# that and leave it alone; nothing here is asked to hold that promise, only
# to watch it kept over several passes rather than one.
STARTS_BEFORE=$(grep -c 'started hang · implement' "$E2E_DISPATCH_LOG" 2>/dev/null || true)
PASSES_BEFORE=$(grep -c 'lanes still working\|nothing to do' "$E2E_DISPATCH_LOG" 2>/dev/null || true)
passed_twice_more() {
  local now
  now=$(grep -c 'lanes still working\|nothing to do' "$E2E_DISPATCH_LOG" 2>/dev/null || true)
  [ "${now:-0}" -ge "$(( ${PASSES_BEFORE:-0} + 2 ))" ]
}
if poll_until 15 passed_twice_more; then
  STARTS_AFTER=$(grep -c 'started hang · implement' "$E2E_DISPATCH_LOG" 2>/dev/null || true)
  PID_NOW=$(cat "$SPOOLWAY_PROJECT_HOME/headless/hang · implement.pid" 2>/dev/null || true)
  if [ "$STARTS_AFTER" = "$STARTS_BEFORE" ] && [ "$PID_NOW" = "$PID1" ] && kill -0 "$PID1" 2>/dev/null; then
    ok "a restarted dispatcher leaves a live lane alone instead of starting a second"
  else
    bad "a restarted dispatcher leaves a live lane alone instead of starting a second"
    printf '        starts before/after: %s/%s, pid then/now: %s/%s\n' \
      "$STARTS_BEFORE" "$STARTS_AFTER" "$PID1" "$PID_NOW"
  fi
else
  bad "a restarted dispatcher leaves a live lane alone instead of starting a second (no second pass seen)"
fi

# `hang`'s own lane is done being useful — every case above has read what it
# needed to. Swept here, with the teardown on, so it is not still asleep five
# minutes from now while later cases run, and forgotten outright — `hang`
# never reports, so left in the queue it is only ever restarted for nothing.
sweep
forget hang

# ------------------------------------------------ a report, nobody listening
# `orphan` is an ordinary fast task: its implement lane reports in
# milliseconds. A sixty-second interval on the dispatcher that starts it
# turns the race between "the report lands" and "the next pass would read
# it" into a minute of slack — plenty to kill the dispatcher in between and
# still be certain the report happened with nobody running.
SAVED_INTERVAL=$E2E_INTERVAL
E2E_INTERVAL=60s
must "the teardown is on, for the ordinary flow" \
  "$SPOOLWAY" config set dispatch.tear_lanes_on_stop true
dispatcher_start
task_doc "$LIVE/orphan.md" orphan "$BODY" "group: disaster" "touches: [notes/orphan.md]"
must "orphan queues" "$SPOOLWAY" queue add --from "$LIVE/orphan.md"
if drive orphan review 20; then
  kill_dispatcher KILL
  STAGE_ORPHANED=$(stage_of orphan)
  E2E_INTERVAL=$SAVED_INTERVAL
  dispatcher_start
  if [ "$STAGE_ORPHANED" = review ] && drive orphan document 20; then
    ok "a lane that reported with no dispatcher up is picked up on the next pass"
  else
    bad "a lane that reported with no dispatcher up is picked up on the next pass"
    printf '        stage while orphaned: %s, stage now: %s\n' "$STAGE_ORPHANED" "$(stage_of orphan)"
  fi
else
  E2E_INTERVAL=$SAVED_INTERVAL
  bad "a lane that reported with no dispatcher up is picked up on the next pass (never reached review)"
fi
sweep

# ------------------------------------------------------- a stop, teardown on
must "the teardown is on" "$SPOOLWAY" config set dispatch.tear_lanes_on_stop true
queue_hang stop-on
OS5=$(one_shot_start)
PID5=$(lane_pid "stop-on · implement" 20)
WT5=$(worktree_of stop-on)
if [ -n "$PID5" ] && [ -n "$WT5" ]; then
  one_shot_stop "$OS5"
  if ! kill -0 "$PID5" 2>/dev/null && [ ! -e "$WT5" ] \
     && git rev-parse --verify -q "refs/heads/task/stop-on" >/dev/null 2>&1; then
    ok "a stop ends the lane before it removes the checkout under it"
  else
    bad "a stop ends the lane before it removes the checkout under it"
    printf '        lane alive: %s, worktree: %s, branch: %s\n' \
      "$(kill -0 "$PID5" 2>/dev/null && echo yes || echo no)" \
      "$([ -e "$WT5" ] && echo present || echo gone)" \
      "$(git rev-parse --verify -q refs/heads/task/stop-on >/dev/null 2>&1 && echo present || echo gone)"
  fi
else
  bad "a stop ends the lane before it removes the checkout under it (the lane never started)"
  one_shot_stop "$OS5"
fi
# Swept already — the assertion above is what the sweep left behind. Only
# forgotten here, so a later case's dispatcher never restarts it for nothing.
forget stop-on

# ------------------------------------------------------ a stop, teardown off
must "the teardown is off" "$SPOOLWAY" config set dispatch.tear_lanes_on_stop false
queue_hang stop-off
OS6=$(one_shot_start)
PID6=$(lane_pid "stop-off · implement" 20)
WT6=$(worktree_of stop-off)
if [ -n "$PID6" ] && [ -n "$WT6" ]; then
  one_shot_stop "$OS6"
  if kill -0 "$PID6" 2>/dev/null && [ -d "$WT6" ] && [ "$(stage_of stop-off)" = implement ]; then
    ok "tear_lanes_on_stop = false leaves every worktree exactly where it stood"
  else
    bad "tear_lanes_on_stop = false leaves every worktree exactly where it stood"
    printf '        lane alive: %s, worktree: %s, stage: %s\n' \
      "$(kill -0 "$PID6" 2>/dev/null && echo yes || echo no)" \
      "$([ -d "$WT6" ] && echo present || echo gone)" "$(stage_of stop-off)"
  fi
else
  bad "tear_lanes_on_stop = false leaves every worktree exactly where it stood (the lane never started)"
  one_shot_stop "$OS6"
fi
# Left alive on purpose above — the sweep below is what finally takes it, now
# that the assertion it was kept for has run.
sweep
forget stop-off

# --------------------------------------------------------- a multiplexer dies
# `src/tmux.rs` drives the default server unless `SPOOLWAY_TMUX_SOCKET` names
# one of its own, which is the whole of what lets this run a real tmux
# without ever touching a person's own — see `Tmux::new`.
if ! command -v tmux >/dev/null 2>&1; then
  echo "  skipped — no \`tmux\` on PATH, and this case needs a real server"
else
  SOCK="$LIVE/tmux.sock"
  export SPOOLWAY_TMUX_SOCKET="$SOCK"
  must "the tmux backend" "$SPOOLWAY" config set dispatch.backend tmux
  # Split, so this task cuts a session of its own — the shape
  # `Mux::reopen_owned_pane`'s heal path is written against, and the one the
  # unit test beside it already covers.
  must "tmux runs split" "$SPOOLWAY" config set dispatch.tmux_mode split

  # An ordinary fast task, not a `hang` one — and not for the reason `hang`
  # is wrong everywhere else in this suite. `Mux::start_lane` respawns a
  # pane as the agent itself, so the pane closes the moment that process
  # does; `agents/pi`'s fast steps exit in milliseconds either way. What
  # matters here is *which* lane the kill lands on. `implement`'s own launch
  # counts against `attempts`, and a task's `MAX_LAUNCHES` is one — the same
  # one bound `check_unreported` reads as "an agent that died at launch" — so
  # a multiplexer killed under a lane already charged against that count
  # reads as exactly that on the very next pass, whether or not a
  # multiplexer had anything to do with it, and the task is handed to a
  # person rather than healed. The heal this case is about runs on a fresh
  # launch, one that has not spent its attempt yet — which for a real task is
  # any step arriving after the one whose session went stale, so the kill
  # lands between `implement` reporting and `review` starting, on a
  # workspace `review` has not tried to use yet.
  task_doc "$LIVE/tmux-heal.md" tmux-heal "$BODY" "group: disaster" "touches: [notes/tmux-heal.md]"
  must "tmux-heal queues" "$SPOOLWAY" queue add --from "$LIVE/tmux-heal.md"
  dispatcher_restart
  if drive tmux-heal review 20; then
    WT7=$(worktree_of tmux-heal)
    tmux -S "$SOCK" kill-server 2>/dev/null
    # `document` rather than watching for `review`'s own pane: the mock
    # reports in milliseconds, so by the time anything here could poll for
    # it the pane healed to run it is already closed again, same as
    # `implement`'s was. Stage reaching past `review` at all is only
    # possible if that launch — the one this kill left with a stale
    # workspace and nothing else — opened a fresh pane rather than failing;
    # `handover` and `checks` need a real forge this suite has none of, so
    # `document` is as far as an unrelated limitation leaves provable.
    if [ -n "$WT7" ] && drive tmux-heal document 30 && [ -d "$WT7" ]; then
      ok "a killed multiplexer clears the ids on file and the next launch opens new ones"
    else
      bad "a killed multiplexer clears the ids on file and the next launch opens new ones"
      printf '        worktree: %s, stage: %s\n' \
        "$([ -d "$WT7" ] && echo present || echo gone)" "$(stage_of tmux-heal)"
    fi
  else
    bad "a killed multiplexer clears the ids on file and the next launch opens new ones (never reached review)"
  fi
  sweep
  tmux -S "$SOCK" kill-server 2>/dev/null || true
  unset SPOOLWAY_TMUX_SOCKET
  must "back to headless" "$SPOOLWAY" config set dispatch.backend headless
fi

# ------------------------- a pass problem reaches the project log, not the board
# `sweep` first: the resident supervisor `dispatcher_start` keeps running is
# always `--plain`, and this case is about the one thing that differs when a
# board is drawn instead — nothing here works if the resident dispatcher
# might answer for `problem-log` before the board-mode one-shot below gets to
# it.
sweep
task_doc "$LIVE/problem-log.md" problem-log "$BODY" "group: disaster" \
  "touches: [notes/problem-log.md]"
must "problem-log queues" "$SPOOLWAY" queue add --from "$LIVE/problem-log.md"
# A stage no pipeline defines is the cheapest real problem to manufacture:
# `Dispatcher::pass` hits it on the very first pass, with nothing else to set
# up first — the "sitting on `stage`, which pipeline ... does not define" arm
# in `src/dispatch.rs`.
sed -i 's/^stage: .*/stage: not-a-real-step/' "$SPOOLWAY_PROJECT_HOME/queue/problem-log.md"

PROJECT_LOG="$HOME/.spoolway/logs/$(basename "$SPOOLWAY_PROJECT_HOME").log"
rm -f "$PROJECT_LOG"
# However many lines the shared dispatch log already carries — only what
# lands after this point is this case's own to judge.
BEFORE_LINES=$(wc -l < "$E2E_DISPATCH_LOG" 2>/dev/null || echo 0)

BOARD_PID=$(one_shot_start_board)
if wait_for_text 20 "$PROJECT_LOG" 'not-a-real-step'; then
  ok "a pass problem is appended to the project's own log"
else
  bad "a pass problem is appended to the project's own log"
fi
one_shot_stop "$BOARD_PID"

if ! tail -n +"$((BEFORE_LINES + 1))" "$E2E_DISPATCH_LOG" | grep -q '  ! '; then
  ok "and no problem line reaches the board's own output"
else
  bad "and no problem line reaches the board's own output"
  tail -n +"$((BEFORE_LINES + 1))" "$E2E_DISPATCH_LOG" | sed 's/^/        /'
fi
forget problem-log

# ------------------- an unparsable queue file no longer freezes the pass
# `load_dir` used to return the first parse error for the whole directory, so
# one task hand-edited and left without its closing `---` failed every pass,
# every `spoolway report` and the board at once. Now the bad file is skipped
# and named in the project log, and everything else still moves.
sweep
PROJECT_LOG="$HOME/.spoolway/logs/$(basename "$SPOOLWAY_PROJECT_HOME").log"
rm -f "$PROJECT_LOG"
queue_hang survivor
printf -- '---\nid: broken\nstage: queued\n' \
  > "$SPOOLWAY_PROJECT_HOME/queue/broken.md"

BROKEN_PID=$(one_shot_start)
SURVIVOR_PID=$(lane_pid "survivor · implement" 25)
if [ -n "$SURVIVOR_PID" ] && wait_for_text 20 "$PROJECT_LOG" 'broken.md'; then
  ok "a broken queue file is skipped and named, and the rest of the queue still runs"
else
  bad "a broken queue file is skipped and named, and the rest of the queue still runs"
  printf '        survivor stage: %s, lane pid: %s\n' \
    "$(stage_of survivor)" "${SURVIVOR_PID:-none}"
  grep -i broken "$PROJECT_LOG" 2>/dev/null | sed 's/^/        log: /'
fi
one_shot_stop "$BROKEN_PID"
rm -f "$SPOOLWAY_PROJECT_HOME/queue/broken.md"
forget survivor

finish
