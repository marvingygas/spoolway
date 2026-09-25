#!/usr/bin/env bash
# The board's confirm-panel keys, driven end to end: real keystrokes read off
# a real pipe by a real `spoolway dispatch`, over a real headless lane that is
# genuinely mid-turn.
#
# Everything else about the board is rendering, and `run.sh` says why that is
# not worth a suite. Two things here are the exception. Answering a panel
# *routes*: a keypress interrupts a live lane, or writes or moves a task
# file, and the panel standing between the two is the only thing that stops
# it — none of which a frame comparison can see. And the header's restart
# hint is not rendering either: it is a real dispatcher resolving `spoolway`
# off a real `PATH` and running it, which is the one part of the board no
# unit test can stand up — the last section covers it.
#
# `p` and `P` carry most of the suite,
# since pausing is the one panel that can also abort a live lane; `U` and a
# `p` of a task that never started each get one pass through the same
# `enter`/`esc` answers near the bottom, to cover the other panels and the
# `parked_from` record a park off `queued` leaves; `R` proves it sends that
# same kind of row straight back to `queued`, dependency or not, still
# gating on a real one beside it. Last of all is the pass-yields section,
# which queues six more hang lanes of its own — placed after `R` simply so
# it does not disturb the state `R`'s own assertions read, not because
# anything here is scarce enough to make the order matter.
#
# How a key gets in. The board reads stdin itself, between redraws — both
# while the dispatcher is waiting out its interval and, since the pass-yields
# task, between a pass's own units of work too, so a key answers at the same
# rate whether the queue is busy or idle — so this suite runs a
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
SOLUTIONS="$LIVE/solutions"

# A canned patch that cannot apply, so `implement` reports `--fail` for real
# rather than the marker-file work every other task here gets — the one thing
# the schedule-catches-a-fail scenario near the bottom needs, and the reason
# this suite installs its agents with a solutions directory at all.
install_agents "$LIVE/bin" "$CTL" "$SOLUTIONS"
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
  setsid bash -c 'echo $$ >"$1"; exec "$2" dispatch <"$3"' \
    _ "$pidfile" "$SPOOLWAY" "$BOARD_FIFO" \
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

# How many frames the board has drawn so far. `Board::draw` (src/status/mod.rs)
# writes this exact clear-and-home sequence, unconditionally, once every poll
# slice — about once a second — whether or not the frame it drew differs from
# the last, which is what makes counting them a real clock rather than a
# sleep of another name: it advances on its own pace, never faster and never
# slower than the board this suite is actually watching.
_frame_count() { grep -aoF $'\x1b[2J\x1b[H' "$BOARD_LOG" 2>/dev/null | wc -l; }
_frame_past()  { [ "$(_frame_count)" -gt "$1" ]; }

# Wait for the board to draw at least one frame after this call started —
# proof a keystroke just sent was actually read and acted on (the loop that
# reads a key redraws again immediately after applying it), not a guess at
# how long that takes.
next_frame() {
  local secs=${1:-10} before
  before=$(_frame_count)
  poll_until "$secs" _frame_past "$before"
}

# `next_frame`, `n` times over — the pacing a case proving *nothing* happened
# needs: long enough that a change reacting late would still land inside the
# window, and no longer than the board actually takes to get there.
settle_frames() {
  local n=$1 secs=${2:-10} i
  for ((i = 0; i < n; i++)); do
    next_frame "$secs" || return 1
  done
}

# The other half: the board is drawing frames continuously, so "it stopped
# saying that" means the frames drawn *from here on* do not say it. Two real
# frames' worth of wait, then read only what landed after the first of them.
stops_drawing() {
  local what=$1 unwanted=$2 mark
  settle_frames 1
  mark=$(wc -l < "$BOARD_LOG")
  settle_frames 1
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

# The positive twin of `never_draws`: what the board has drawn *since* a mark
# says it. The log is one file for the suite's whole life, so a plain `draws`
# of anything a header has ever said would pass on a frame from ten minutes
# and three dispatchers ago.
draws_since() {
  local what=$1 wanted=$2 mark=$3
  if tail -n "+$((mark + 1))" "$BOARD_LOG" | grep -qF -- "$wanted"; then ok "$what"
  else
    bad "$what (no frame since said \"$wanted\")"
    tail -30 "$BOARD_LOG" | sed 's/^/        /'
  fi
}

# The polling twin of `draws_since`, for a string that has not appeared yet
# and needs the wait itself, not just the check after it — `draws` cannot be
# used instead, because that reads the whole log and would pass on a frame
# from before the mark, drawn by a `PATH` executable this suite never put
# there.
_since_says() {
  local mark=$1 want=$2
  tail -n "+$((mark + 1))" "$BOARD_LOG" | grep -qF -- "$want"
}
draws_since_waited() {
  local what=$1 want=$2 mark=$3 secs=${4:-20}
  if poll_until "$secs" _since_says "$mark" "$want"; then ok "$what"
  else
    bad "$what (no frame since said \"$want\")"
    tail -30 "$BOARD_LOG" | sed 's/^/        /'
  fi
}

# And its negative. `never_draws` above runs the same test, but reports a
# failure as a panel having opened, which is the wrong thing to say about a
# header — so the header's checks carry their own wording rather than making
# the panel helper's vaguer to fit them both.
draws_since_not() {
  local what=$1 unwanted=$2 mark=$3
  if tail -n "+$((mark + 1))" "$BOARD_LOG" | grep -qF -- "$unwanted"; then
    bad "$what (the board drew \"$unwanted\")"
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

# The cursor starts on the first row on its own now — mid-turn is the only
# row here — so `p` reaches it with no `down` needed first.
press p

draws "\`p\` over a live lane opens a panel naming the task" "pause mid-turn"
draws "the panel names the step and calls it an agent turn" "implement    agent"
draws "and says the turn is only interrupted" "The turn is interrupted, not killed."
draws "and offers enter on its own line" "[enter] pause it"
draws "with schedule and cancel on the line under it" "[s] schedule   [esc] cancel"
stage_stays "the task file is untouched while the panel is open" mid-turn implement
if kill -0 "$LANE_PID" 2>/dev/null; then ok "and the lane is still running"
else bad "and the lane is still running"; fi

# A key that is neither leaves the panel open and acts on nothing.
press x
settle_frames 2
stage_stays "a stray key over the panel changes nothing" mid-turn implement
draws "and the panel is still up" "pause mid-turn"

press $'\x1b'
stops_drawing "esc closes the panel" "pause mid-turn"
stage_stays "and leaves the task where it was" mid-turn implement
if kill -0 "$LANE_PID" 2>/dev/null; then ok "and leaves the lane running"
else bad "and leaves the lane running"; fi

# `s` leaves the turn running and writes a schedule instead of an interrupt —
# the NEXT column carries it, and the task's own file gains `gate_at`.
press p
next_frame
press s
stops_drawing "\`s\` closes the panel without aborting anything" "pause mid-turn"
stage_stays "the task stays right where it was" mid-turn implement
has "and gains a schedule naming its own step" "gate_at: implement" \
  "$SPOOLWAY_PROJECT_HOME/queue/mid-turn.md"
draws "and the NEXT column says where it is headed" "paused after implement"
if kill -0 "$LANE_PID" 2>/dev/null; then ok "and the lane is still running"
else bad "and the lane is still running"; fi

# Pressing `s` again, over a fresh panel on the same still-live step, clears
# the schedule it just wrote.
press p
next_frame
press s
stops_drawing "pressing \`s\` again closes the panel too" "pause mid-turn"
lacks "and clears the schedule it named" "gate_at:" \
  "$SPOOLWAY_PROJECT_HOME/queue/mid-turn.md"

press p
next_frame
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
draws "and offers enter on its own line" "[enter] pause the run"
draws "with schedule and cancel on the line under it" "[s] schedule   [esc] cancel"
stage_stays "no task file is written while the panel is open" busy implement
stage_stays "not even the idle one" behind queued

press $'\x1b'
stops_drawing "esc closes the run-wide panel" "Pausing aborts"
stage_stays "and leaves the live task where it was" busy implement
if kill -0 "$BUSY_PID" 2>/dev/null; then ok "and leaves its lane running"
else bad "and leaves its lane running"; fi

# `s` on the run-wide panel schedules the one task with something live and
# parks the rest of the run at once — there is no step in flight to wait out
# for those.
press P
next_frame
press s
stops_drawing "\`s\` closes the run-wide panel without aborting anything" "Pausing aborts"
stage_stays "the live task keeps running" busy implement
has "and gains a schedule naming its own step" "gate_at: implement" \
  "$SPOOLWAY_PROJECT_HOME/queue/busy.md"
stage_reaches "the idle task parks at once, exactly as enter would" behind paused 25
if kill -0 "$BUSY_PID" 2>/dev/null; then ok "and the live lane is still running"
else bad "and the live lane is still running"; fi

# What a scheduled pause does once the step it names actually passes is
# `commands::report`'s own road, covered there — this suite owns the
# keypress alone, so the rest of the run is parked outright the ordinary way.
press P
next_frame
press $'\r'
stage_reaches "enter parks the task that was live" busy paused 25

# ------------------------------------------ `P` with nothing live at all
# Nothing is running now, so there is nothing to confirm: the keypress parks
# on the spot and no panel is drawn at all.
queue_idle late busy
next_frame
MARK=$(wc -l < "$BOARD_LOG")
press P
stage_reaches "\`P\` with nothing live parks the run at once" late paused 25
never_draws "with no panel to answer" "Pausing aborts" "$MARK"

# ----------------------------------------------- a paused row is a no-op
MARK=$(wc -l < "$BOARD_LOG")
press p
settle_frames 2
never_draws "\`p\` over an already-paused row opens nothing" "Pausing aborts" "$MARK"

# ------------------------------- pausing a task that never started at all
# Nothing live, nothing declared for `queued` in any pipeline, so this parks
# on the spot exactly like the "nothing live" `P` above — the one thing worth
# checking here is the record it leaves, not the keypress.
queue_idle never-run late
next_frame
MARK=$(wc -l < "$BOARD_LOG")
press P
stage_reaches "pausing a task that never started parks it at once" never-run paused 25
never_draws "with no panel to answer" "Pausing aborts" "$MARK"
lacks "and writes no \`parked_from\` for a task that was still \`queued\`" \
  "parked_from:" "$SPOOLWAY_PROJECT_HOME/queue/never-run.md"

# A task that never started has no step to go back to, and the entry is the
# wrong answer: it would skip the dependency gate only `queued` applies. So
# it goes back onto `queued` — and stays there, because `late`, which it
# depends on, is still parked above.
must "resuming it by hand" "$SPOOLWAY" resume never-run
stage_reaches "it goes back onto queued, gated again by its paused dependency" never-run queued 25
lacks "carrying no leftover \`parked_from\`" "parked_from:" \
  "$SPOOLWAY_PROJECT_HOME/queue/never-run.md"
lacks "or \`resume:\`" "resume:" "$SPOOLWAY_PROJECT_HOME/queue/never-run.md"

# --------------------------------------- `U` answers only to enter now
# The other panels this task hands the same two answers `p`/`P`'s own already
# had: `U` still opens unconditionally, but the letter that opened it no
# longer closes it — only `enter` does, the same rule proven above for the
# pause panel. Two tasks sit on `queued` now: `stalled`, and `never-run`
# back where its resume put it.
queue_idle stalled late
next_frame
press U
draws "\`U\` opens the unqueue-all panel" "2 tasks have not started:"
draws "answered with enter or esc, and nothing else" "[enter] unqueue them   [esc] cancel"
stage_stays "nothing is written while the panel is open" stalled queued

press U
settle_frames 2
stage_stays "the old confirming letter no longer answers the panel" stalled queued
draws "and the panel is still up" "2 tasks have not started:"

_gone() { [ ! -e "$SPOOLWAY_PROJECT_HOME/queue/$1.md" ]; }
press $'\r'
if poll_until 25 _gone stalled; then ok "enter carries out the unqueue"
else bad "enter carries out the unqueue"; tail -30 "$BOARD_LOG" | sed 's/^/        /'; fi
has "the document lands back in pending" "id: stalled" "$SPOOLWAY_PROJECT_HOME/pending/stalled.md"
has "and so does the one that never ran" "id: never-run" "$SPOOLWAY_PROJECT_HOME/pending/never-run.md"

# ------------------------------------------- `u` carries a dependent chain
# The reach this task adds: `u` on a task a still-queued task depends on no
# longer refuses outright — it lists the whole chain of unstarted tasks that
# reach it through `depends_on` and carries every one of them back to
# pending together. Restarted fresh, with its own group sorted ahead of
# every other row's `board`, so the freshly restarted board's own cursor
# opens on it directly — the same trick the blocked-row section below uses
# for the same reason.
#
# `chain-head` depends on `chain-gate`, a `hang`-mode lane of its own — a
# dependency cannot cross a group (`queue add` refuses it), so gating
# `chain-head` on something still paused elsewhere in the run, the way
# `queue_idle` does for every other idle task here, is not available inside
# a fresh group of its own. A live lane that never returns is: with nothing
# to make it `Done`, `chain-head` never becomes ready, and is still
# genuinely `queued` — not already off running its own pipeline — when `u`
# is pressed. The cursor already opens on `chain-gate`, the first row of its
# own fresh group; `chain-gate` sorts above `chain-head` within it, so it
# takes one `down` rather than two to reach the row `u` is this section's
# own.
board_stop
mkdir -p "$CTL"
echo hang > "$CTL/chain-gate"
task_doc "$LIVE/chain-gate.md" chain-gate "$BODY" "group: 0-chain" \
  "touches: [notes/chain-gate.md]"
must "chain-gate queues" "$SPOOLWAY" queue add --from "$LIVE/chain-gate.md"
task_doc "$LIVE/chain-head.md" chain-head "$BODY" "group: 0-chain" \
  "touches: [notes/chain-head.md]" "depends_on: [chain-gate]"
must "chain-head queues" "$SPOOLWAY" queue add --from "$LIVE/chain-head.md"
task_doc "$LIVE/chain-tail.md" chain-tail "$BODY" "group: 0-chain" \
  "touches: [notes/chain-tail.md]" "depends_on: [chain-head]"
must "chain-tail queues" "$SPOOLWAY" queue add --from "$LIVE/chain-tail.md"
board_start

CHAIN_GATE_PID=$(lane_pid "chain-gate · implement" 30)
if [ -n "$CHAIN_GATE_PID" ]; then ok "the gate lane is really mid-turn"
else bad "the gate lane is really mid-turn"; fi
draws "the board draws the chain" "chain-tail"

press $'\x1b[B'
next_frame
press u

draws "\`u\` on the chain's head opens a panel naming both" "unqueue chain-head"
draws "the dependent, marked with what it depends on" \
  "chain-tail   (depends on chain-head)"
draws "and offers enter for both, esc to cancel" "[enter] unqueue them   [esc] cancel"
stage_stays "nothing moves while the panel is open" chain-head queued
stage_stays "not even the dependent" chain-tail queued

press $'\r'
if poll_until 25 _gone chain-head; then ok "enter carries the head back to pending"
else bad "enter carries the head back to pending"; tail -30 "$BOARD_LOG" | sed 's/^/        /'; fi
if poll_until 25 _gone chain-tail; then ok "and carries the dependent along with it"
else bad "and carries the dependent along with it"; tail -30 "$BOARD_LOG" | sed 's/^/        /'; fi
has "the head's document lands back in pending" "id: chain-head" \
  "$SPOOLWAY_PROJECT_HOME/pending/chain-head.md"
has "and so does the dependent's" "id: chain-tail" \
  "$SPOOLWAY_PROJECT_HOME/pending/chain-tail.md"

# ------------------------------------ `p` reaches a blocked row with a live lane
#
# The reach this task adds: the unblocker can be mid-turn on a `blocked` row
# exactly as an implementer can be mid-turn on `implement`, once the run is
# unattended, and `p` interrupts that turn the same way. This needs a
# restart — config is read once at launch, not on every pass — so this task
# is queued straight into the live queue directory, on `blocked` already,
# with `blocked_from` set the way a real block leaves it; `routines.sh` and
# `trials.sh` write straight into the live queue directory with `task_doc`
# the same way, for a fixture no dispatcher needs to walk there itself. Its
# group sorts ahead of every other row's `board`, so the freshly restarted
# board's own cursor opens directly on it, whatever else in this run is
# still parked — no `down` needed to reach it.
#
# The unblocker is pointed at the `pi` profile rather than the shipped
# `claude` one: the `hang` ctl mode this suite relies on everywhere else is
# `pi`'s own protocol (see `scripts/e2e/agents/pi`) — the `claude` stand-in
# reads the same ctl file for one mode of its own, `decline`, and answers
# with a real pass for anything else, `hang` included.
board_stop
must "unattended, so \`blocked\` is staffed by the unblocker" \
  "$SPOOLWAY" config set unattended.enabled true
must "the unblocker's agent, so its own \`hang\` ctl mode is honoured" \
  "$SPOOLWAY" config set unattended.blocked_agent pi
echo hang > "$CTL/stuck"
task_doc "$SPOOLWAY_PROJECT_HOME/queue/stuck.md" stuck "$BODY" \
  "stage: blocked" "blocked_from: implement" "group: 0-blocked" \
  "touches: [notes/stuck.md]"
board_start

STUCK_PID=$(lane_pid "stuck · blocked" 30)
if [ -n "$STUCK_PID" ]; then ok "the unblocker is really mid-turn on the blocked row"
else bad "the unblocker is really mid-turn on the blocked row"; fi
draws "the board draws the staffed blocked row" "stuck"

press p

draws "\`p\` over the blocked row opens a panel naming the task" "pause stuck"
draws "the panel names the blocked step and calls it an agent turn" "blocked    agent"
stage_stays "the task file is untouched while the panel is open" stuck blocked

press $'\r'
stage_reaches "enter parks the blocked task, interrupting its live unblocker" stuck paused 25
if poll_while 15 kill -0 "$STUCK_PID"; then ok "and the unblocker's turn is over"
else bad "and the unblocker's turn is over"; fi
has "parked_from names the blocked step it was pulled off of" "parked_from: blocked" \
  "$SPOOLWAY_PROJECT_HOME/queue/stuck.md"
has "blocked_from survives the park untouched, beside it" "blocked_from: implement" \
  "$SPOOLWAY_PROJECT_HOME/queue/stuck.md"

# Resuming it puts it back on the step it was parked from — `blocked` — with
# the same session carried forward, not `resume_target`'s ordinary road. No
# "freed stale lane" line is expected here: headless's own interrupt above
# already dropped the lane's record whole rather than leaving it settled (see
# `pressing_shift_p_opens_a_panel_over_a_live_headless_lane_and_only_enter_interrupts_it`
# in `src/status/mod.rs`), so there is nothing left for `free_stale_lanes` to
# find.
RESUME_OUT=$("$SPOOLWAY" resume stuck 2>&1)
if [ $? -eq 0 ]; then ok "resuming the parked blocked task"
else bad "resuming the parked blocked task"; sed 's/^/        /' <<<"$RESUME_OUT"; fi
if grep -qF "stuck: -> blocked" <<<"$RESUME_OUT"; then
  ok "it goes back onto \`blocked\`, the step it was parked from"
else
  bad "it goes back onto \`blocked\`, the step it was parked from"
  sed 's/^/        /' <<<"$RESUME_OUT"
fi
stage_reaches "the task lands back on \`blocked\`, not \`resume_target\`'s entry" stuck blocked 25

# ------------------------- a schedule catches a failing step, not only a pass
# `s` above already proved it writes and clears `gate_at`; what a schedule
# does once the step it names actually fails, rather than passes, is
# `commands::report`'s own road, not a keypress this suite can watch — so
# this drives it with a canned patch that cannot apply, the mock's own way of
# making `implement` report `--fail` for real rather than the marker-file
# work every other task above got.
mkdir -p "$SOLUTIONS/pause-fail-catch"
printf 'not a real patch\n' > "$SOLUTIONS/pause-fail-catch/implement.patch"
task_doc "$LIVE/pause-fail-catch.md" pause-fail-catch "$BODY" "group: board" \
  "touches: [notes/pause-fail-catch.md]" "gate_at: implement"
must "pause-fail-catch queues" "$SPOOLWAY" queue add --from "$LIVE/pause-fail-catch.md"

# `implement`'s own `on_fail` is `blocked` by default, so without the
# schedule this fail would land there directly. With it, the schedule catches
# the fail before that ever happens — the task pauses instead.
stage_reaches "a schedule catches a failing step rather than letting it fall to on_fail" \
  pause-fail-catch paused 30
has "and paused_at names the step that actually failed" "paused_at: implement" \
  "$SPOOLWAY_PROJECT_HOME/queue/pause-fail-catch.md"
lacks "with the schedule spent, not standing" "gate_at:" \
  "$SPOOLWAY_PROJECT_HOME/queue/pause-fail-catch.md"
draws "and the board's NEXT column names what it caught" "implement blocked → blocked" 30

must "resuming it sends the caught fail on to blocked, exactly where it would have landed unheld" \
  "$SPOOLWAY" resume pause-fail-catch
stage_reaches "and it lands there" pause-fail-catch blocked 25

# --------------------------- a gated stop offers a key before every command
# The mockup this task built: a report that lands a task on `paused` names
# what it offers key first, then the command — never a bare key with nothing
# to run, and never a command with no key in front of it. Proven here
# against the board's own row for the same stop, which is what a person
# still watching the board sees for as long as the pane above stays up: the
# row and the pane always name the same thing. Then `spoolway task edit`,
# run the way a person — or the lane in that pane, once it is stopped —
# would run it: from outside the lane entirely, against a task already
# parked.
task_doc "$LIVE/gate-edit.md" gate-edit "$BODY" "group: board" \
  "touches: [notes/gate-edit.md]" "gate_at: implement"
must "gate-edit queues" "$SPOOLWAY" queue add --from "$LIVE/gate-edit.md"

stage_reaches "a gated pass parks on paused" gate-edit paused 30
# The row's own NEXT text clips to the pane's width like any other cell, so
# this checks the key-first shape rather than the full, possibly-clipped
# command text.
draws "the board's row offers the key before the command it fires" \
  "[r] → review — \`spoolway resume gate" 30

MOCKUP="$LIVE/mockup-section.txt"
printf 'held here for a person, edited from outside the lane\n' > "$MOCKUP"
must "\`task edit\` rewrites the stopped document's own section" \
  "$SPOOLWAY" task edit gate-edit --section Non-goals --from "$MOCKUP"
has "the change is on disk" "held here for a person, edited from outside the lane" \
  "$SPOOLWAY_PROJECT_HOME/queue/gate-edit.md"
stage_stays "and the task is still paused, untouched by the edit" gate-edit paused

# The one action left after an edit — resuming — is still named key first,
# then the command: the same rule every other choice a stop offers follows.
says "and names the one action still left, key first" \
  "resume   [r]   spoolway resume gate-edit" \
  "$SPOOLWAY" task edit gate-edit --section Goal --from "$MOCKUP"

# `--from -`, the stream the mockup itself is drawn with — a shape nothing
# else here drives, so this is the one proof it works at all.
says "\`--from -\` reads the new section from standard input" \
  "\`## Goal\` rewritten, 1 lines" \
  bash -c "printf 'read from stdin\n' | '$SPOOLWAY' task edit gate-edit --section Goal --from -"
has "and the stdin content lands on disk" "read from stdin" \
  "$SPOOLWAY_PROJECT_HOME/queue/gate-edit.md"

# --------------------------------- `R` sends a queued park back to `queued`
# The reach this task adds: the run-wide resume key reaches a row parked off
# `queued` itself exactly as it reaches a real step, and does not hold it for
# a dependency the way a real step's row still would. `behind` and `late`
# have sat on `paused` since `P` first parked them, both still gated by
# `busy` — still paused itself, and never resumed by anything above — so
# neither dependency has finished. `gate-edit` is still paused too, a real
# gate, so this also proves `R`'s panel still gates on it the same as ever
# while the queued parks beside it need no such asking. `mid-turn` and
# `busy` go past `R`'s own panel back onto `implement` here too, so their
# `hang`-mode lanes are running again once this section ends — the
# pass-race section after this one queues six more of its own regardless,
# since nothing in this fixture caps `agents.<profile>.concurrency` or a
# model's `slots` (both default to `0`, uncapped — `src/config.rs`), so
# there is no worker-slot budget here for a later section to run short of.
press R
draws "\`R\` still gates on the one real gate among the parks" "resume all"
draws "naming it, not the queued parks beside it" "gate-edit"
press $'\r'
stage_reaches "a row parked off \`queued\` goes back to \`queued\`, dependency or not" \
  late queued 25
stage_reaches "and every other queued park along with it" behind queued 25
lacks "carrying no leftover \`parked_from\`" "parked_from:" \
  "$SPOOLWAY_PROJECT_HOME/queue/late.md"

# --------------------------------- a park typed while six lanes race to start
# `pass` now calls back into the board between its own units of work — see
# `Dispatcher::pass` — so a keypress can land while a pass is still working
# through the queue, not only once it returns and the run settles into its
# interval wait. Six lanes launching in the very same pass (each a real
# worktree cut and a real, if fake, agent start) makes a mid-pass read
# likely, but this cannot *prove* the dispatcher was still busy the moment
# `P` was read — `Phase::Passing` and `Phase::Waiting` draw identically on
# purpose (`src/status/mod.rs`), so nothing on screen tells the two apart,
# and the task itself is why: no suite can time a keypress against a pass's
# own read. What this does cover end to end is the park itself, landing
# either way without being lost — the mid-pass overwrite that could lose it
# — `persist_task`'s own later write of a task it read before the park
# lands — is proven at the unit level, by
# `persist_task_does_not_overwrite_a_park_typed_mid_pass` in
# `src/dispatch.rs`; this is a real keystroke, read off a real pipe,
# answered by a real dispatcher, reaching a real task document either way.
#
# `P` rather than a single row's `p`, so this does not also depend on the
# cursor's position among the dozens of rows the suite has already built —
# `on_key` routes both through the same `park_under_lock`, so the write
# path this is proving is identical either way.
for n in 0 1 2 3 4 5; do
  queue_hang "pass-race-$n"
done
# Whether any of the six is already confirmed live by the moment this lands
# is exactly the race this section exists to not care about: nothing live
# yet parks the whole run on `P` alone, same as the "nothing live" case
# above; anything already live opens the same confirm panel the "one lane
# live" case above does, and the `enter` right behind `P` answers it. Both
# are read either way — in one drain by the pass's own callback if `P`
# lands mid-pass, a poll slice apart by the wait loop (which reads at most
# one key per slice) if it lands during the wait instead — so sending them
# back to back is not a bet on which of the two reads it.
press P
press $'\r'
# The longest wait in this suite, and the one place the default `20` is too
# thin: the park lands only once the key is read, and the pass it has to be
# read inside is cutting six real worktrees. `30` on the first assertion,
# which is the one that waits on the read at all; `25` — the margin every
# other park here already takes — on the five that follow, since one `P`
# parks them all and they are on disk by the time the first one is.
stage_reaches "\`P\`, typed the instant six lanes race to start, still parks them" \
  pass-race-0 paused 30
for n in 1 2 3 4 5; do
  stage_reaches "and every lane racing to start beside it" "pass-race-$n" paused 25
done

# ------------------------------- the version in the header, and the restart
# The header names the build the dispatcher is *running*, which after an
# install is no longer the file on disk — and that gap is the whole reason
# the hint exists. It cannot be proven anywhere but here: it takes a real
# dispatcher process, a real `PATH`, and a real executable answering
# `--version` on it. Two runs, one fake each, because the hint is as much
# about when it stays off as when it appears.
#
# The fake answers `--version` and hands everything else to the build under
# test, so putting it first on `PATH` changes nothing else in the suite. A
# lane reaches the real binary through `E2E_SPOOLWAY` regardless.
VBIN="$LIVE/version-bin"
install_fake_spoolway() {
  local version=$1
  mkdir -p "$VBIN"
  cat > "$VBIN/spoolway" <<EOF
#!/usr/bin/env bash
if [ "\$1" = --version ]; then echo "spoolway $version"; exit 0; fi
exec "$SPOOLWAY" "\$@"
EOF
  chmod 755 "$VBIN/spoolway"
}
export PATH="$VBIN:$PATH"

# What the build under test calls itself, read from it rather than written
# down here — a version bump must not need an edit in this suite.
RUNNING_VERSION=$("$SPOOLWAY" --version | awk '{print $NF}')

# An older executable on `PATH`: the version shows, the hint does not. This
# is the ordinary case — somebody has spoolway installed and the dispatcher
# is running the same build or a newer one.
install_fake_spoolway 0.0.1
board_stop
board_start
# Everything below reads only what this board drew. Four frames rather than
# one, because the reading of the `PATH` executable lands on a thread behind
# the first redraw — so a hint that was going to appear has had every chance
# to by the time the negative assertions run.
OLD_MARK=$(wc -l < "$BOARD_LOG")
settle_frames 4
draws_since "the header names the running build" "· v$RUNNING_VERSION" "$OLD_MARK"
draws_since_not "and no longer counts the run's own clock" " · up " "$OLD_MARK"
draws_since_not "an older executable on PATH adds no restart hint" \
  "restart to use latest installed version" "$OLD_MARK"

# And a newer one: the same header, with the hint after it. Scoped to a mark
# and polled, rather than a plain `draws` — a machine running this suite with
# its own newer spoolway already on PATH would have carried the label on
# frames drawn before this fake ever went up, and a plain `draws` would pass
# on one of those without this fake having proven anything.
install_fake_spoolway 99.0.0
board_stop
board_start
NEW_MARK=$(wc -l < "$BOARD_LOG")
draws_since_waited "a newer executable on PATH asks for a restart" \
  "restart to use latest installed version" "$NEW_MARK"
draws_since "with the running build still the version it names" \
  "· v$RUNNING_VERSION" "$NEW_MARK"

board_stop
finish
