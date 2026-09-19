#!/usr/bin/env bash
# The ways a run ends badly, held against a real dispatcher and real detached
# lanes — a hard kill with lanes live, the stale lock it leaves, a restart
# over a still-running lane, a lane that reports with nobody listening, and a
# stop that leaves every lane running.
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
# stand-in reads that file before it reads its own task id.
#
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

# Any resident supervisor a case left running, ended hard, so a one-shot pass
# later in the suite never has to race a second dispatcher for the lock. A
# stop no longer gives back anything on its own — see `forget` below for what
# actually reclaims a task's checkout — so there is nothing else left for this
# to do. Not a check itself — nothing here calls `ok` or `bad` — so it costs
# nothing on the tally either way.
sweep() {
  kill_dispatcher KILL
}

# What a person abandoning a task by hand would do — there is no `queue
# remove`, because there is ordinarily no reason to want one, and a stop no
# longer reclaims a checkout either. So this suite's own housekeeping does
# what both used to: the lane's process is killed outright, its worktree and
# branch go back with git directly, and the task file itself leaves the
# queue. Named for the stage the task is still sitting on — a swept task was
# never a *done* one, so its lane keeps the name of the step it stood on
# rather than any step it never reached.
#
# This suite's own `hang`-mode tasks never report, so nothing ever moves them
# on; left in the queue, every pass from here on would restart one for no
# reason a later case is asking about.
forget() {
  local id=$1 lane pid wt
  lane="$id · $(stage_of "$id")"
  pid=$(cat "$SPOOLWAY_PROJECT_HOME/headless/$lane.pid" 2>/dev/null)
  [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null
  wt=$(worktree_of "$id")
  [ -n "$wt" ] && git worktree remove --force "$wt" 2>/dev/null
  git branch -D "task/$id" 2>/dev/null
  rm -f "$SPOOLWAY_PROJECT_HOME/queue/$id.md"
  true
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

# ------------------------------------------------------------- a stop, live
# A stop never removes a worktree, workspace, pane or tab any more — every
# live lane is left exactly where it was.
queue_hang stop-live
OS5=$(one_shot_start)
PID5=$(lane_pid "stop-live · implement" 20)
WT5=$(worktree_of stop-live)
if [ -n "$PID5" ] && [ -n "$WT5" ]; then
  one_shot_stop "$OS5"
  if kill -0 "$PID5" 2>/dev/null && [ -d "$WT5" ] && [ "$(stage_of stop-live)" = implement ]; then
    ok "a stop leaves the lane running and its worktree exactly where it stood"
  else
    bad "a stop leaves the lane running and its worktree exactly where it stood"
    printf '        lane alive: %s, worktree: %s, stage: %s\n' \
      "$(kill -0 "$PID5" 2>/dev/null && echo yes || echo no)" \
      "$([ -d "$WT5" ] && echo present || echo gone)" "$(stage_of stop-live)"
  fi

  # And the next run picks the queue back up: its own reconciliation matches
  # the still-running lane against the task's own stage and leaves it alone,
  # rather than starting a second one over the same worktree — the same
  # promise the earlier "a restart, over a live lane" case holds for a kill.
  STARTS_BEFORE=$(grep -c 'started stop-live · implement' "$E2E_DISPATCH_LOG" 2>/dev/null || true)
  dispatcher_start
  if poll_until 15 bash -c \
       '[ "$(cat "$0/headless/stop-live · implement.pid" 2>/dev/null)" = "$1" ]' \
       "$SPOOLWAY_PROJECT_HOME" "$PID5"; then
    STARTS_AFTER=$(grep -c 'started stop-live · implement' "$E2E_DISPATCH_LOG" 2>/dev/null || true)
    if [ "$STARTS_AFTER" = "$STARTS_BEFORE" ] && kill -0 "$PID5" 2>/dev/null; then
      ok "the next run resumes the same lane rather than starting a second one"
    else
      bad "the next run resumes the same lane rather than starting a second one"
      printf '        starts before/after: %s/%s\n' "$STARTS_BEFORE" "$STARTS_AFTER"
    fi
  else
    bad "the next run resumes the same lane rather than starting a second one (pid file changed)"
  fi
  kill_dispatcher KILL
else
  bad "a stop leaves the lane running and its worktree exactly where it stood (the lane never started)"
  one_shot_stop "$OS5"
fi
sweep
forget stop-live

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

# `problem_log::path` keys this off `$SPOOLWAY_PROJECT_HOME`'s own basename
# now — `<label>-<id>`, not the checkout's plain basename — so it survives
# a rename the same way the home itself does; see `src/problem_log.rs` and
# the `binding-record` task.
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

# --------- the retention sweep spares a queued task's scratch and headless state
# `retain::sweep_now` runs on every command's startup and deletes byproduct
# entries older than `housekeeping.retention_days`. `scratch/<id>` is what a
# lane is handed as `$SPOOLWAY_SCRATCH` — planner output and all — and
# `headless/` holds the record a running lane is read back through. An entry
# in either must survive for as long as its task is still in the queue,
# whatever stage it sits on, or `spoolway resume` comes back to a lane with
# nothing under it.
sweep
must "a short retention window" "$SPOOLWAY" config set housekeeping.retention_days 1
HOME_DIR=$SPOOLWAY_PROJECT_HOME
queue_hang retained
mkdir -p "$HOME_DIR/scratch/retained" "$HOME_DIR/headless"
echo 'planner output' > "$HOME_DIR/scratch/retained/plan.md"
echo '{}' > "$HOME_DIR/headless/retained · implement.json"
# Backdated well past the window — a directory's own mtime does not move while
# a lane only writes files into it, which is the whole trap.
touch -d '3 days ago' \
  "$HOME_DIR/scratch/retained" "$HOME_DIR/scratch/retained/plan.md" \
  "$HOME_DIR/headless/retained · implement.json"
"$SPOOLWAY" queue list >/dev/null 2>&1 || true
if [ -f "$HOME_DIR/scratch/retained/plan.md" ] \
   && [ -f "$HOME_DIR/headless/retained · implement.json" ]; then
  ok "the sweep spares a queued task's scratch and headless state"
else
  bad "the sweep spares a queued task's scratch and headless state"
  printf '        scratch: %s, headless: %s\n' \
    "$([ -f "$HOME_DIR/scratch/retained/plan.md" ] && echo present || echo gone)" \
    "$([ -f "$HOME_DIR/headless/retained · implement.json" ] && echo present || echo gone)"
fi
# Once the task leaves the queue the same aged entries do age out.
forget retained
"$SPOOLWAY" queue list >/dev/null 2>&1 || true
if [ ! -e "$HOME_DIR/scratch/retained" ] \
   && [ ! -e "$HOME_DIR/headless/retained · implement.json" ]; then
  ok "and once its task is gone from the queue they age out as before"
else
  bad "and once its task is gone from the queue they age out as before"
  printf '        scratch: %s, headless: %s\n' \
    "$([ -e "$HOME_DIR/scratch/retained" ] && echo present || echo gone)" \
    "$([ -e "$HOME_DIR/headless/retained · implement.json" ] && echo present || echo gone)"
fi
must "retention back to the default" "$SPOOLWAY" config set housekeeping.retention_days 30

# ------------- the retired local-model warning never draws
# The footer's slot counts only ever see the lanes spoolway started, but that
# gap is exactly as true of a manually started cloud session as a local
# one — see docs/dispatcher.md's own accounting for what "slots" counts. The
# board used to single out a `local = true` model with a standing line saying
# manually started sessions are not considered by the slots pool; it draws no
# such line any more, flagged or not — the pooled-slots line for the model
# itself is a separate thing and is unaffected.
#
# covers: models.<glob>.local — the board never renders the retired manual-session warning, whether or not a queued task's model is flagged local
sweep
LOCAL_MODEL=$(local_model)   # `fake-local`, the model the mock's local steps name
queue_hang localnote          # a task whose default pipeline routes through that step

# Unflagged: no reason to expect the warning, and none appears.
BEFORE=$(wc -l < "$E2E_DISPATCH_LOG" 2>/dev/null || echo 0)
BOARD_PID=$(one_shot_start_board)
poll_until 15 bash -c \
  'tail -n +'"$((BEFORE + 1))"' "'"$E2E_DISPATCH_LOG"'" | grep -q "slots"'
if tail -n +"$((BEFORE + 1))" "$E2E_DISPATCH_LOG" | grep -qF "not considered by the slots pool"; then
  bad "no board frame carries the retired warning (model unflagged)"
  tail -n +"$((BEFORE + 1))" "$E2E_DISPATCH_LOG" | sed 's/^/        /'
else
  ok "no board frame carries the retired warning (model unflagged)"
fi
one_shot_stop "$BOARD_PID"

# Flagged local, and given a pool of its own, `pi` — the profile
# `localnote`'s `implement` step names — still earns a slots line on the
# frame, with no lane of its own ever started: `slots_used` widens
# `agent_model` over every queued task's whole pipeline, not only its live
# lanes. That accounting is untouched by this task; only the warning beside
# it is gone.
must "flag the model local" "$SPOOLWAY" config set "models.$LOCAL_MODEL.local" true
must "give the model a pool" "$SPOOLWAY" config set "models.$LOCAL_MODEL.slots" 3
BEFORE=$(wc -l < "$E2E_DISPATCH_LOG" 2>/dev/null || echo 0)
BOARD_PID=$(one_shot_start_board)
if wait_for_text 20 "$E2E_DISPATCH_LOG" "$LOCAL_MODEL"; then
  ok "the board still names the pooled model's own slots line"
else
  bad "the board still names the pooled model's own slots line"
  tail -n +"$((BEFORE + 1))" "$E2E_DISPATCH_LOG" | sed 's/^/        /'
fi
tail -n +"$((BEFORE + 1))" "$E2E_DISPATCH_LOG" > "$LIVE/local-pool.out"
# The live count in `pi`'s own line is not asserted — `localnote`'s hung
# `implement` lane may or may not have been dispatched yet by this point, and
# either way is beside what this case is about: the `/3` cap is there at all,
# off the queued pipeline's own step, whether or not a lane is live.
if python3 - "$LIVE/local-pool.out" "$LOCAL_MODEL" <<'PY'
import re
import sys

data = open(sys.argv[1], "rb").read()
model = sys.argv[2].encode()
pool_line = re.compile(
    rb"\x1b\[1mpi\s*\x1b\[0m {3}\x1b\[2mslots\x1b\[0m \d+/3 {3}" + re.escape(model)
)
sys.exit(0 if pool_line.search(data) else 1)
PY
then
  ok "the pooled slots line names the flagged model"
else
  bad "the pooled slots line names the flagged model"
  sed 's/^/        /' "$LIVE/local-pool.out"
fi
if grep -qF "not considered by the slots pool" "$LIVE/local-pool.out"; then
  bad "flagging a model local does not bring the retired warning back"
  sed 's/^/        /' "$LIVE/local-pool.out"
else
  ok "flagging a model local does not bring the retired warning back"
fi
one_shot_stop "$BOARD_PID"
forget localnote

# ---------------- `spoolway eval` banks no catch-up line for a live lane
# The settled-lane sweep catches up the turns that arrive in a transcript
# after the dispatcher tore its lane down. A lane still in flight is not
# that: its spend is the dispatcher's to bank at teardown, diffed against a
# snapshot the pass read once, so a line `spoolway eval` slipped in behind
# it is counted twice. The sweep skips any session `lanes.json` still names
# — held here against a real dispatcher with a real lane asleep, and a
# ledger line already naming that session so the sweep would otherwise read
# its transcript and bank the delta.
sweep
echo 4000 > "$CTL/transcript"   # the mock writes a transcript for its lanes
dispatcher_start
queue_hang evallive
EVAL_PID=$(lane_pid "evallive · implement" 20)
poll_until 15 lane_on_record "evallive · implement"
SID=$(sed -n 's/.*"session": *"\([^"]*\)".*/\1/p' \
        "$SPOOLWAY_PROJECT_HOME/lanes.json" | head -1)
if [ -n "$EVAL_PID" ] && [ -n "$SID" ]; then
  # A line as though a prior step of this same session had already been
  # banked, dated far in the past so only the live-lane guard — not the
  # sweep's mtime gate — is what keeps the (newer) transcript unread.
  printf '{"ts":"2020-01-01T00:00:00+00:00","task":"evallive","step":"implement","pipeline":"disaster","agent":"pi","kind":"pi","model":"fake-local","session":"%s","turns":1,"tokens":{"input":10,"output":16}}\n' \
    "$SID" >> "$SPOOLWAY_PROJECT_HOME/usage.jsonl"
  BEFORE=$(grep -c "\"session\":\"$SID\"" "$SPOOLWAY_PROJECT_HOME/usage.jsonl")
  # `eval` itself has to succeed: a crash or refusal leaves the ledger
  # untouched too, and asserting "no line appeared" over that would pass for
  # the wrong reason.
  if "$SPOOLWAY" eval >/dev/null 2>&1; then
    AFTER=$(grep -c "\"session\":\"$SID\"" "$SPOOLWAY_PROJECT_HOME/usage.jsonl")
    if [ "$AFTER" = "$BEFORE" ]; then
      ok "\`spoolway eval\` banks no catch-up line for a lane still in flight"
    else
      bad "\`spoolway eval\` banks no catch-up line for a lane still in flight"
      printf '        ledger lines for %s before/after: %s/%s\n' "$SID" "$BEFORE" "$AFTER"
    fi
  else
    bad "\`spoolway eval\` exited non-zero — cannot judge whether its sweep banked a line"
  fi
else
  bad "\`spoolway eval\` banks no catch-up line for a lane still in flight (the lane never started)"
fi
: > "$CTL/transcript"
sweep
forget evallive

# ---------------- a home deleted by hand refuses, naming both files
# `binding-record`'s own Goal names three ways the checkout's stamp and its
# home's record can stop agreeing, and says each one "stops with an error
# naming both files instead of quietly starting an empty queue" — this is
# the first of the three, held against a real dispatcher-adjacent command
# the way every other hand-broken state in this suite is, rather than as a
# unit test against a bare fixture `$HOME` (the seven states themselves
# already are — see `src/repo.rs`'s `bind_criterion_*` tests). A separate,
# freshly configured project, so deleting its home cannot disturb any case
# still to come.
new_repo "$LIVE/broken"
configure_project plan/broken
BROKEN_STAMP="$PWD/.git/spoolway-id"
BROKEN_HOME="$SPOOLWAY_PROJECT_HOME"
[ -d "$BROKEN_HOME" ] || {
  printf '  \033[31mSETUP\033[0m the broken-binding case: %s does not exist yet\n' "$BROKEN_HOME" >&2
  exit 2
}
rm -rf "$BROKEN_HOME"
if OUT=$("$SPOOLWAY" queue list 2>&1); then
  bad "a home deleted by hand is refused (it was accepted)"
  printf '%s\n' "$OUT" | sed 's/^/        /'
elif grep -qF "$BROKEN_STAMP" <<<"$OUT" && grep -qF "$BROKEN_HOME/project.toml" <<<"$OUT"; then
  ok "a home deleted by hand is refused, naming both the stamp and the record"
else
  bad "a home deleted by hand is refused, naming both the stamp and the record"
  printf '%s\n' "$OUT" | sed 's/^/        /'
fi

# ---------------- a 0.2 home is migrated onto its clone's id, once
# `migrate-legacy-home`: everybody upgrading from 0.2 has a home filed under
# their checkout's basename, with a `project.toml` naming only a `root` — no
# id, since the stamp this project keys a home on now did not exist yet. A
# real dispatcher and a real headless lane are started first, normally,
# against a freshly initialised project — then the checkout's own stamp and
# its home directory are torn down into that exact 0.2 shape *out from
# under* both, still running. Neither the dispatcher process nor the lane's
# own shell cares that its files moved to a different path partway through;
# what changes is only what a separate `spoolway queue list` call, run
# afterwards, finds when it goes looking for a live holder there — which is
# the whole point: this is a real, live dispatcher and a real, live lane,
# not a pid standing in for one.
#
# The two liveness signals are proven apart, not just together: a live
# dispatcher's own lock is checked first inside `migrate_legacy_home`, so
# leaving both live at once would only ever prove the dispatcher's half —
# the refusal text would say so either way, and a passing check that never
# actually exercised the lane side would be silent about it. So the
# dispatcher is stopped first, on its own, and the still-running lane is
# proven to keep blocking the move by itself before it too is torn down. A
# separate project again, for the same reason `broken` above is one.
new_repo "$LIVE/legacy" plan/legacy
configure_project plan/legacy
LEGACY_ID_HOME="$SPOOLWAY_PROJECT_HOME"
LEGACY_HOME="$HOME/.spoolway/$(basename "$PWD")"
[ -d "$LEGACY_ID_HOME" ] || {
  printf '  \033[31mSETUP\033[0m the legacy-home case: %s does not exist yet\n' "$LEGACY_ID_HOME" >&2
  exit 2
}

dispatcher_start
queue_hang legacyhang
LEGACY_LANE_PID=$(lane_pid "legacyhang · implement" 20)
LEGACY_WT=$(worktree_of legacyhang)
if [ -z "$LEGACY_LANE_PID" ] || [ -z "$LEGACY_WT" ]; then
  bad "a legacy home refuses to move while a real dispatcher is live (the lane never started)"
else
  # What a 0.2 checkout actually looked like: no stamp of its own, and its
  # state filed under the plain basename rather than an id-keyed one.
  rm -f .git/spoolway-id .git/spoolway-label
  mv "$LEGACY_ID_HOME" "$LEGACY_HOME"
  printf 'root = "%s"\n' "$PWD" > "$LEGACY_HOME/project.toml"
  # `worktree_path:` names a location under the just-moved directory's old
  # name — repointed at the same inode's new path, the same rename every
  # worktree under a real legacy home would carry. The lane process's own
  # `/proc/<pid>/cwd` needs no such repointing: the kernel already resolves
  # it to wherever the directory holding it now lives.
  NEW_WT="$LEGACY_HOME/${LEGACY_WT#"$LEGACY_ID_HOME"/}"

  # Phase one: dispatcher and lane both still alive. `Lock::holder` is
  # checked before any worktree, so this proves the dispatcher's own half
  # of the refusal specifically.
  if OUT=$("$SPOOLWAY" queue list 2>&1); then
    bad "a legacy home refuses to move while a real dispatcher is live (it was accepted)"
    printf '%s\n' "$OUT" | sed 's/^/        /'
  elif grep -qF "cannot move while work is live" <<<"$OUT" \
       && grep -qF "dispatcher running" <<<"$OUT" \
       && grep -qF "$LEGACY_HOME" <<<"$OUT" \
       && [ -f "$LEGACY_HOME/project.toml" ] \
       && [ ! -e "$LEGACY_ID_HOME" ]; then
    ok "a legacy home refuses to move while a real dispatcher is live, untouched"
  else
    bad "a legacy home refuses to move while a real dispatcher is live, untouched"
    printf '%s\n' "$OUT" | sed 's/^/        /'
  fi

  # Phase two: the dispatcher stops, and the lane it started keeps running
  # regardless — the same survival every other kill in this suite counts
  # on. With no dispatcher lock left to answer for it, the move now refuses
  # (or does not) on the lane alone.
  dispatcher_stop
  if OUT=$("$SPOOLWAY" queue list 2>&1); then
    bad "a legacy home refuses to move while its lane alone is still live (it was accepted)"
    printf '%s\n' "$OUT" | sed 's/^/        /'
  elif grep -qF "cannot move while work is live" <<<"$OUT" \
       && grep -qF "checked out" <<<"$OUT" \
       && ! grep -qF "dispatcher running" <<<"$OUT" \
       && [ -f "$LEGACY_HOME/project.toml" ] \
       && [ ! -e "$LEGACY_ID_HOME" ]; then
    ok "a legacy home refuses to move while its lane alone is still live, untouched"
  else
    bad "a legacy home refuses to move while its lane alone is still live, untouched"
    printf '%s\n' "$OUT" | sed 's/^/        /'
  fi

  # End the lane itself — its whole process group, not just the recorded
  # wrapper pid: `setsid` gives it one of its own on purpose (the same
  # separation every other lane this suite kills a dispatcher around
  # relies on), and the stand-in agent can be a child of that wrapper
  # rather than the same pid, left running — and so still holding a live
  # `/proc/<pid>/cwd` under the worktree — by a plain single-pid kill.
  kill -9 -- "-$LEGACY_LANE_PID" 2>/dev/null
  poll_while 10 kill -0 -- "-$LEGACY_LANE_PID"
  git worktree remove --force "$NEW_WT" 2>/dev/null
  git branch -D task/legacyhang 2>/dev/null

  MIGRATED_HOME=""
  if OUT=$("$SPOOLWAY" queue list 2>&1); then
    # `LEGACY_ID_HOME` named the home before this checkout's own rollback
    # and is stale now — the move mints this checkout a fresh id, same as
    # any other first resolution, so the migrated home is a different
    # directory than the one this case tore down by hand above.
    MIGRATED_HOME=$(find "$HOME/.spoolway" -maxdepth 1 -type d \
                      -name "$(basename "$PWD")-*" | head -1)
    if [ ! -e "$LEGACY_HOME" ] \
       && [ -n "$MIGRATED_HOME" ] \
       && [ -f "$MIGRATED_HOME/project.toml" ] \
       && [ -f "$MIGRATED_HOME/queue/legacyhang.md" ]; then
      ok "the same command moves the legacy home, queue and all, once the dispatcher and lane have stopped"
    else
      bad "the same command moves the legacy home, queue and all, once the dispatcher and lane have stopped"
      printf '        legacy home gone: %s, migrated home: %s\n' \
        "$([ ! -e "$LEGACY_HOME" ] && echo yes || echo no)" "${MIGRATED_HOME:-none}"
    fi
  else
    bad "the same command moves the legacy home, queue and all, once the dispatcher and lane have stopped"
    printf '%s\n' "$OUT" | sed 's/^/        /'
  fi
  [ -n "$MIGRATED_HOME" ] && rm -f "$MIGRATED_HOME/queue/legacyhang.md" 2>/dev/null
fi

finish
