#!/usr/bin/env bash
# The board's confirm-panel keys, driven end to end: real keystrokes read off
# a real pipe by a real `spoolway dispatch`, over a real headless lane that is
# genuinely mid-turn.
#
# Everything else about the board is rendering, and `run.sh` says why that is
# not worth a suite. This is the exception: answering a panel *routes*. A
# keypress interrupts a live lane, or writes or moves a task file, and the
# panel standing between the two is the only thing that stops it — none of
# which a frame comparison can see. `p` and `P` carry most of the suite,
# since pausing is the one panel that can also abort a live lane; `U` and a
# `p` of a task that never started each get one pass through the same
# `enter`/`esc` answers near the bottom, to cover the other panels and the
# `parked_from` record a park off `queued` leaves.
#
# How a key gets in. The board reads stdin itself, between redraws, only
# while the dispatcher is waiting out its interval — so this suite runs a
# dispatcher of its own with the board drawn rather than `lib.sh`'s shared
# `--plain` supervisor, with a fifo on its stdin. The fifo is opened
# read-write here (`exec 9<>`) so opening it does not block on a reader, and
# stays open for the suite's whole life so the board never reads EOF and
# stops listening.
#
# What is asserted is the two things a keypress leaves behind: the panel text
# in the board's own output, which is redirected to a file here, and the
# `stage:` in the task document. A live lane is the `hang` mode `agents/pi`
# already has — it sleeps for five minutes and never reports — reached by
# seeding `$CTL/<task>` before the task is queued, exactly as `disaster.sh`
# does.
#
# No `covers:` tag — the coverage map enumerates `config.toml` keys and
# pipeline step keys, and a board key is neither.
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
configure_project plan/board "$LIVE/worktrees"

BODY="$LIVE/body.md"
task_body "$BODY"

BOARD_LOG="$LIVE/board.out"
BOARD_FIFO="$LIVE/keys"
BOARD_PID=""
: > "$BOARD_LOG"

# queue_hang <task-id> [extra frontmatter...]
#
# A task whose `implement` lane never returns on its own, so the row it draws
# is genuinely mid-turn for as long as this suite needs it.
queue_hang() {
  local id=$1; shift
  mkdir -p "$CTL"
  echo hang > "$CTL/$id"
  task_doc "$LIVE/$id.md" "$id" "$BODY" "group: board" "touches: [notes/$id.md]" "$@"
  must "$id queues" "$SPOOLWAY" queue add --from "$LIVE/$id.md"
}

# queue_idle <task-id> <depends-on>
#
# A task that will never start a lane while the task it depends on is
# unfinished — the "nothing live" row `p` and `P` have to park on the spot.
queue_idle() {
  local id=$1 on=$2
  task_doc "$LIVE/$id.md" "$id" "$BODY" "group: board" \
    "touches: [notes/$id.md]" "depends_on: [$on]"
  must "$id queues" "$SPOOLWAY" queue add --from "$LIVE/$id.md"
}

# The dispatcher this suite drives: its own, with the board drawn and a fifo
# on its stdin. `lib.sh`'s supervisor runs `--plain`, which draws no board and
# reads no keys, and restarts on its own — neither of which this wants.
board_start() {
  rm -f "$BOARD_FIFO"
  mkfifo "$BOARD_FIFO" || { echo "no fifo" >&2; exit 2; }
  # Read-write, so this open returns without waiting for the board to open
  # the other end, and the board never sees EOF while the suite still holds
  # it. A plain `exec 9>` would block here until the reader arrived.
  exec 9<>"$BOARD_FIFO"
  local pidfile="$LIVE/board.pid"
  rm -f "$pidfile"
  setsid bash -c 'echo $$ >"$1"; exec "$2" dispatch --interval "$3" <"$4"' \
    _ "$pidfile" "$SPOOLWAY" "${E2E_INTERVAL:-1s}" "$BOARD_FIFO" \
    >>"$BOARD_LOG" 2>&1 &
  disown
  poll_until 10 test -s "$pidfile" || {
    printf '  \033[31mSETUP\033[0m the board dispatcher never started\n' >&2
    exit 2
  }
  BOARD_PID=$(cat "$pidfile")
}

board_stop() {
  local pid=$BOARD_PID
  BOARD_PID=""
  [ -n "$pid" ] || return 0
  kill -TERM -- "-$pid" 2>/dev/null
  poll_while 5 kill -0 -- "-$pid"
  kill -KILL -- "-$pid" 2>/dev/null
  return 0
}
trap board_stop EXIT

# One keystroke into the board's stdin. `\x1b` for esc and `\r` for enter are
# what a terminal actually sends, and `src/screen.rs` decodes them the same
# way whether the descriptor behind them is a tty or this pipe.
press() { printf '%s' "$1" >&9; }

# Wait for the board to draw something, up to `secs` seconds. A frame lands
# about once a second, so nothing here is a fixed sleep.
board_says() { wait_for_text "${2:-20}" "$BOARD_LOG" "$1"; }

# says, for the board's own output.
draws() {
  local what=$1 want=$2 secs=${3:-20}
  if board_says "$want" "$secs"; then ok "$what"
  else bad "$what (the board never drew \"$want\")"; tail -30 "$BOARD_LOG" | sed 's/^/        /'; fi
}

# The other half: the board is drawing frames continuously, so "it stopped
# saying that" means the frames drawn *from here on* do not say it. Two
# frames' worth of wait, then read only what landed after it.
stops_drawing() {
  local what=$1 unwanted=$2 mark
  sleep 3
  mark=$(wc -l < "$BOARD_LOG")
  sleep 3
  if tail -n "+$((mark + 1))" "$BOARD_LOG" | grep -qF -- "$unwanted"; then
    bad "$what (the board is still drawing \"$unwanted\")"
    tail -30 "$BOARD_LOG" | sed 's/^/        /'
  else ok "$what"; fi
}

# And the same shape for a panel that must never have opened at all: nothing
# after this mark says it.
never_draws() {
  local what=$1 unwanted=$2 mark=$3
  if tail -n "+$((mark + 1))" "$BOARD_LOG" | grep -qF -- "$unwanted"; then
    bad "$what (a panel opened: \"$unwanted\")"
    tail -30 "$BOARD_LOG" | sed 's/^/        /'
  else ok "$what"; fi
}

stage_stays() {
  local what=$1 task=$2 want=$3
  if [ "$(stage_of "$task")" = "$want" ]; then ok "$what"
  else bad "$what ($task is at $(stage_of "$task"), wanted $want)"; fi
}

_stage_is() { [ "$(stage_of "$1")" = "$2" ]; }
stage_reaches() {
  local what=$1 task=$2 want=$3
  if poll_until "${4:-20}" _stage_is "$task" "$want"; then ok "$what"
  else bad "$what ($task is at $(stage_of "$task"), wanted $want)"; fi
}

# ------------------------------------------- `p` over a live agent lane
# The cursor's own panel: it names the step, calls it an agent turn, and says
# the turn is interrupted rather than killed — the one thing a person
# answering `enter` needs to know before they do.
queue_hang mid-turn
board_start
LANE_PID=$(lane_pid "mid-turn · implement" 30)
if [ -n "$LANE_PID" ]; then ok "the lane is really mid-turn"
else bad "the lane is really mid-turn"; fi
draws "the board draws the run" "mid-turn"

# One `down` puts the cursor on the first row, which is the only row here.
press $'\x1b[B'
sleep 2
press p

draws "\`p\` over a live lane opens a panel naming the task" "pause mid-turn"
draws "the panel names the step and calls it an agent turn" "implement    agent"
draws "and says the turn is only interrupted" "The turn is interrupted, not killed."
draws "answered with enter or esc, and nothing else" "[enter] pause it   [esc] cancel"
stage_stays "the task file is untouched while the panel is open" mid-turn implement
if kill -0 "$LANE_PID" 2>/dev/null; then ok "and the lane is still running"
else bad "and the lane is still running"; fi

# A key that is neither leaves the panel open and acts on nothing.
press x
sleep 3
stage_stays "a stray key over the panel changes nothing" mid-turn implement
draws "and the panel is still up" "pause mid-turn"

press $'\x1b'
stops_drawing "esc closes the panel" "pause mid-turn"
stage_stays "and leaves the task where it was" mid-turn implement
if kill -0 "$LANE_PID" 2>/dev/null; then ok "and leaves the lane running"
else bad "and leaves the lane running"; fi

press p
sleep 2
press $'\r'
stage_reaches "enter parks the task" mid-turn paused 25
if poll_while 15 kill -0 "$LANE_PID"; then ok "and the turn it named is over"
else bad "and the turn it named is over"; fi

# ---------------------------------- `P` with one lane live and one task idle
# The run-wide panel lists every abort and counts what pauses behind it, so
# the whole of what the keypress does is on screen before it happens.
queue_hang busy
queue_idle behind busy
BUSY_PID=$(lane_pid "busy · implement" 30)
if [ -n "$BUSY_PID" ]; then ok "a second lane is mid-turn"
else bad "a second lane is mid-turn"; fi

press P
draws "\`P\` opens one panel for the run" "Pausing aborts 1 running step:"
draws "naming the task and step it aborts" "busy · implement"
draws "and counting what pauses behind it" "1 more task pauses with nothing"
draws "answered with enter or esc, and nothing else" "[enter] pause the run   [esc] cancel"
stage_stays "no task file is written while the panel is open" busy implement
stage_stays "not even the idle one" behind queued

press $'\x1b'
stops_drawing "esc closes the run-wide panel" "Pausing aborts"
stage_stays "and leaves the live task where it was" busy implement
if kill -0 "$BUSY_PID" 2>/dev/null; then ok "and leaves its lane running"
else bad "and leaves its lane running"; fi

press P
sleep 2
press $'\r'
stage_reaches "enter parks the task that was live" busy paused 25
stage_reaches "and the one that had nothing to interrupt" behind paused 25

# ------------------------------------------ `P` with nothing live at all
# Nothing is running now, so there is nothing to confirm: the keypress parks
# on the spot and no panel is drawn at all.
queue_idle late busy
sleep 2
MARK=$(wc -l < "$BOARD_LOG")
press P
stage_reaches "\`P\` with nothing live parks the run at once" late paused 25
never_draws "with no panel to answer" "Pausing aborts" "$MARK"

# ----------------------------------------------- a paused row is a no-op
MARK=$(wc -l < "$BOARD_LOG")
press p
sleep 3
never_draws "\`p\` over an already-paused row opens nothing" "Pausing aborts" "$MARK"

# ------------------------------- pausing a task that never started at all
# Nothing live, nothing declared for `queued` in any pipeline, so this parks
# on the spot exactly like the "nothing live" `P` above — the one thing worth
# checking here is the record it leaves, not the keypress.
queue_idle never-run late
sleep 2
MARK=$(wc -l < "$BOARD_LOG")
press P
stage_reaches "pausing a task that never started parks it at once" never-run paused 25
never_draws "with no panel to answer" "Pausing aborts" "$MARK"
lacks "and writes no \`parked_from\` for a task that was still \`queued\`" \
  "parked_from:" "$SPOOLWAY_PROJECT_HOME/queue/never-run.md"

must "resuming it by hand" "$SPOOLWAY" resume never-run
stage_reaches "it lands on the pipeline's own entry step" never-run implement 25
lacks "carrying no leftover \`parked_from\`" "parked_from:" \
  "$SPOOLWAY_PROJECT_HOME/queue/never-run.md"
lacks "or \`resume:\`" "resume:" "$SPOOLWAY_PROJECT_HOME/queue/never-run.md"

# --------------------------------------- `U` answers only to enter now
# The other panels this task hands the same two answers `p`/`P`'s own already
# had: `U` still opens unconditionally, but the letter that opened it no
# longer closes it — only `enter` does, the same rule proven above for the
# pause panel.
queue_idle stalled late
sleep 2
press U
draws "\`U\` opens the unqueue-all panel" "1 task has not started:"
draws "answered with enter or esc, and nothing else" "[enter] unqueue them   [esc] cancel"
stage_stays "nothing is written while the panel is open" stalled queued

press U
sleep 3
stage_stays "the old confirming letter no longer answers the panel" stalled queued
draws "and the panel is still up" "1 task has not started:"

_gone() { [ ! -e "$SPOOLWAY_PROJECT_HOME/queue/$1.md" ]; }
press $'\r'
if poll_until 25 _gone stalled; then ok "enter carries out the unqueue"
else bad "enter carries out the unqueue"; tail -30 "$BOARD_LOG" | sed 's/^/        /'; fi
has "the document lands back in pending" "id: stalled" "$SPOOLWAY_PROJECT_HOME/pending/stalled.md"

board_stop
finish
