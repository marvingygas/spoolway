#!/usr/bin/env bash
# The board's options menu and confirm panels, driven end to end: real keystrokes read off
# a real pipe by bare `spoolway`'s dispatch tab, dispatching through a real
# `spoolway dispatch` child, over a real headless lane that is genuinely
# mid-turn.
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
# Row actions are reached through `o`, which opens a menu of what the
# cursor's row can take: open, pause, restart, unqueue. The menu runs the
# action's own check again on `enter`, so each section picks its item and
# then answers the panel it opens. `p`, `s`, `u` and `U` are no longer board
# keys; `U` gets one pass that proves it does nothing, and
# `spoolway queue unqueue --all` does its job.
#
# Open is checked once, for the popup a refused open shows: an editor that
# is not on `PATH` is refused before anything is started.
#
# Pausing from the menu and the dispatch tab's stop popup carry most of the
# suite, since pausing and stopping with `i` are the two answers that can
# also abort a live lane; a park off `queued` is checked
# for the `parked_from` record it leaves; `r` proves it sends that
# same kind of row straight back to `queued`, dependency or not, and opens
# the step picker on a real gate beside it. Restart from the menu restarts a
# parked row's step from its own panel and is checked for the new session the
# step comes back on, then restarts a running command step from the panel's
# command form and is checked for the second run. Last of all is the pass-yields
# section, which queues six more hang lanes of its own — placed after the
# `r` section simply so it does not disturb the state that section's own
# assertions read, not because anything here is scarce enough to make the
# order matter.
#
# How a key gets in. The dispatch tab reads stdin between redraws, and the
# dispatcher it started runs as a child of its own, so a key answers at the
# same rate whether the queue is busy or idle. So this suite keeps bare
# `spoolway` open on that tab rather than running `lib.sh`'s shared
# supervisor, with a fifo on its stdin — `lib.sh`'s `screen_start`, which
# holds the fifo open for as long as the screen is up so it never reads EOF
# and stops listening.
#
# What is asserted is the two things a keypress leaves behind: the panel text
# in the board's own frames, which `script` writes to a file here, and the
# `stage:` in the task. A live lane is the `hang` mode `agents/pi`
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
configure_project plan/board

BODY="$LIVE/body.md"
task_body "$BODY"

BOARD_LOG="$LIVE/board.out"
: > "$BOARD_LOG"

# queue_hang <task-id> [extra frontmatter...]
#
# A task whose `implement` lane never returns on its own, so the row it draws
# is genuinely mid-turn for as long as this suite needs it.
#
# In a group of its own, named after itself: a group is one chain, and these
# are unrelated tasks queued while earlier ones still sit parked, so a shared
# group would make each a second root that `queue add` refuses.
queue_hang() {
  local id=$1; shift
  mkdir -p "$CTL"
  echo hang > "$CTL/$id"
  task_doc "$LIVE/$id.md" "$id" "$BODY" "group: $id" "$@"
  must "$id queues" "$SPOOLWAY" queue add --from "$LIVE/$id.md"
}

# queue_idle <task-id> <depends-on>
#
# A task that will never start a lane while the task it depends on is
# unfinished — a row with nothing live, which a stop's `i` leaves alone.
#
# In the group of the task it depends on, read off that task's own file, and
# always on that group's last task: a group is one chain, so every idle task
# here extends the one line `busy` starts rather than fanning off it.
queue_idle() {
  local id=$1 on=$2 group
  group=$(sed -n 's/^group: *//p' "$SPOOLWAY_PROJECT_HOME/queue/$on.md")
  task_doc "$LIVE/$id.md" "$id" "$BODY" "group: $group" \
    "depends_on: [$on]"
  must "$id queues" "$SPOOLWAY" queue add --from "$LIVE/$id.md"
}

# The board this suite drives: bare `spoolway`'s dispatch tab, kept open by
# `lib.sh`'s `screen_start` with a fifo on its stdin and dispatching through
# the child its `enter` starts. `lib.sh`'s resident supervisor runs a plain
# `spoolway dispatch`, which draws no board and reads no keys.
board_start() {
  screen_start "$BOARD_LOG"
  screen_dispatch
}

board_stop() { screen_stop; }

# ------------------------------------------------------- what is on screen now
#
# Every key below is sent only once the screen shows the state the key before
# it left — never on the strength of "a frame was drawn since". The board
# draws for reasons of its own (a lane's TIME ticking, a reading landing a
# moment after a key asked for it), so "some frame landed after the key" is
# not proof the key was read: a frame queued just before it satisfies the wait
# too. A key sent on that false proof lands in whatever the earlier key opens
# a moment later — a `↓` meant for the board moves a picker's cursor instead,
# off the row it preselected. Waiting for the frame that *shows* the result
# makes each key's effect a fact before the next one is typed, and costs only
# as long as the board takes to draw it rather than a frame's worth of polling
# bound per key.

# The frame on screen now, ANSI stripped: the last *complete* frame in the
# log. A frame is written in more than one piece, so the very last one may
# still be arriving; it is complete once it carries the frame writer's own
# closing code (`src/screen/frame_writer.rs`'s `FRAME_END`), and until then
# the one before it is what is on screen. Found by byte offset, for the reason
# `screen_last_frame` in lib.sh gives.
_frame_now() {
  local start=$'\x1b[?2026h\x1b[H' end=$'\x1b[?2026l' offs n from frame
  mapfile -t offs < <(grep -aboF -- "$start" "$BOARD_LOG" 2>/dev/null | cut -d: -f1)
  n=${#offs[@]}
  [ "$n" -gt 0 ] || return 0
  from=${offs[n - 1]}
  frame=$(tail -c "+$((from + 1))" "$BOARD_LOG")
  if [[ $frame != *"$end"* ]] && [ "$n" -gt 1 ]; then
    from=${offs[n - 2]}
    frame=$(tail -c "+$((from + 1))" "$BOARD_LOG" | head -c "$((offs[n - 1] - from))")
  fi
  printf '%s\n' "$frame" | _strip_ansi
}
_now_says()    { grep -qF -- "$1" < <(_frame_now); }
_now_matches() { grep -qE -- "$1" < <(_frame_now); }

# Wait until the screen shows `want` now, up to `secs` seconds.
shows() { poll_until "${2:-20}" _now_says "$1"; }

# `draws`, for the screen as it stands rather than anything the log has ever
# held — a panel that has to *still* be up, not merely to have been drawn.
draws_now() {
  local what=$1 want=$2 secs=${3:-20}
  if shows "$want" "$secs"; then ok "$what"
  else bad "$what (the screen does not show \"$want\")"; _frame_now | sed 's/^/        /'; fi
}

# The frame on screen once it shows `want`, waited for up to `secs` seconds,
# for a caller with more than one thing to read off that same frame.
frame_showing() {
  local want=$1 secs=${2:-20} i frame
  for ((i = 0; i < secs * 10; i++)); do
    frame=$(_frame_now)
    if grep -qF -- "$want" <<<"$frame"; then
      printf '%s\n' "$frame"
      return 0
    fi
    sleep 0.1
  done
  _frame_now
  return 1
}

# The task the board's cursor sits on now — the row its gutter marks with
# `▸` (`src/status/view.rs`'s `CURSOR_MARK`).
_cursor() {
  local at
  at=$(grep -oE '│ ▸ [^ ]+' < <(_frame_now) | head -1)
  printf '%s' "${at#│ ▸ }"
}
_cursor_left() { [ "$(_cursor)" != "$1" ]; }

# cursor_to <task>
#
# Walk the board's cursor onto `task`'s row, one arrow at a time, each sent
# only once the screen shows the last one moved the cursor. The direction is
# read off the frame each time round, so a row order that changes under the
# walk — a resume re-sorting a group — only costs it a step. Returns 1, with
# the cursor wherever it got to, if the row is not on the board.
cursor_to() {
  local want=$1 i frame cur at to
  for ((i = 0; i < 40; i++)); do
    frame=$(_frame_now)
    cur=$(grep -oE '│ ▸ [^ ]+' <<<"$frame" | head -1)
    cur=${cur#│ ▸ }
    [ "$cur" = "$want" ] && return 0
    at=$(grep -nF '│ ▸ ' <<<"$frame" | head -1 | cut -d: -f1)
    to=$(grep -nE "│ (▸ |  )$want " <<<"$frame" | head -1 | cut -d: -f1)
    if [ -z "$at" ] || [ -z "$to" ]; then
      sleep 0.1
      continue
    fi
    if [ "$to" -gt "$at" ]; then press $'\x1b[B'; else press $'\x1b[A'; fi
    poll_until 10 _cursor_left "$cur" || return 1
  done
  printf '  \033[31mlost\033[0m    the cursor never reached %s\n' "$want" >&2
  return 1
}

# The item the open options menu's cursor is on.
_menu_choice() {
  local at
  at=$(grep -oE '│  > [a-z]+ ' < <(_frame_now) | head -1)
  at=${at#│  > }
  printf '%s' "${at% }"
}
_menu_left() { [ "$(_menu_choice)" != "$1" ]; }

# pick_item <item>
#
# Open the options menu on the cursor's row and take `item` — open, pause,
# restart or unqueue — off it by name. Which items a menu lists depends on
# the row, so the walk down it reads the menu's own cursor rather than
# counting. Each key waits for the screen to show the last one landed, which
# also keeps `o` and the arrow behind it from being read as one sequence.
pick_item() {
  local item=$1 i sel
  press o
  poll_until 10 _now_says "│  > open " || return 1
  for ((i = 0; i < 4; i++)); do
    sel=$(_menu_choice)
    if [ "$sel" = "$item" ]; then
      press $'\r'
      return 0
    fi
    press $'\x1b[B'
    poll_until 10 _menu_left "$sel" || return 1
  done
  printf '  \033[31mlost\033[0m    the menu offered no %s\n' "$item" >&2
  return 1
}

# One keystroke into the board. `\x1b` for esc and `\r` for enter are what a
# terminal actually sends, and `src/screen.rs` decodes them the same way
# whether the descriptor behind them is a tty or this pipe.
press() { screen_press "$1"; }

# Wait for the board to draw something, up to `secs` seconds. A frame lands
# about once a second, so nothing here is a fixed sleep.
board_says() { wait_for_text "${2:-20}" "$BOARD_LOG" "$1"; }

# says, for the board's own output.
draws() {
  local what=$1 want=$2 secs=${3:-20}
  if board_says "$want" "$secs"; then ok "$what"
  else bad "$what (the board never drew \"$want\")"; tail -30 "$BOARD_LOG" | sed 's/^/        /'; fi
}

# How many frames the board has drawn so far. The dispatch tab's `draw_board`
# (src/screen/shell.rs) writes this exact start code once every poll slice —
# about once a second — whenever the frame it drew differs from the last one
# painted, or the pane itself resized; a genuinely unchanged tick writes
# nothing at all (`src/screen/frame_writer.rs`'s own skip). `next_frame` and
# `settle_frames` below still work either way: a real change — a keystroke,
# a lane moving — repaints at once, so waiting for the next frame after one
# is still proof it landed; waiting through a stretch with nothing to show
# now spends the whole of `poll_until`'s own bound instead of the one frame
# it used to see, which only costs this suite time, not correctness.
# Every read of the log since a mark below feeds `grep -q` through `< <(…)`,
# never a pipe. `grep -q` stops at its first match, and under `pipefail` the
# SIGPIPE that gives a `tail` still writing a frame would read the match as a
# miss — see `says` in lib.sh.
_frame_count() { grep -aoF $'\x1b[?2026h\x1b[H' "$BOARD_LOG" 2>/dev/null | wc -l; }
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
# window, and no longer than the board actually takes to get there while
# something is still moving. A board with nothing left to show does not draw
# at all any more (see `_frame_count`), so the first `next_frame` to find no
# new frame spends its whole `secs` and fails, and `settle_frames` returns
# there rather than trying the rest: `settle_frames 15` on an idle board is
# one `secs` of waiting (10s by default), not fifteen frames' worth, and
# returns 1. Callers here ignore that status — the wait itself is the point.
settle_frames() {
  local n=$1 secs=${2:-10} i
  for ((i = 0; i < n; i++)); do
    next_frame "$secs" || return 1
  done
}

# The other half: a real change repaints the board at once, so "it stopped
# saying that" is the screen as it stands no longer saying it. Every caller
# sends its key with the thing in question on screen — the panel was waited
# for before it was answered — so the frame that drops it can only be one
# drawn after the key was read. It used to take two frames' worth of pacing
# and read whatever landed after the first, which on an idle board was two
# whole `next_frame` bounds spent on a panel that had closed at once.
stops_drawing() {
  local what=$1 unwanted=$2
  if poll_while 20 _now_says "$unwanted"; then ok "$what"
  else
    bad "$what (the board is still drawing \"$unwanted\")"
    tail -30 "$BOARD_LOG" | sed 's/^/        /'
  fi
}

# And the same shape for a panel that must never have opened at all: nothing
# after this mark says it.
never_draws() {
  local what=$1 unwanted=$2 mark=$3
  if grep -qF -- "$unwanted" < <(tail -n "+$((mark + 1))" "$BOARD_LOG"); then
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
  if grep -qF -- "$wanted" < <(tail -n "+$((mark + 1))" "$BOARD_LOG"); then ok "$what"
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
  grep -qF -- "$want" < <(tail -n "+$((mark + 1))" "$BOARD_LOG")
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
  if grep -qF -- "$unwanted" < <(tail -n "+$((mark + 1))" "$BOARD_LOG"); then
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

# The board's own log holds every frame it ever drew, back to back, each one
# opening with the same start code `_frame_count` already counts by. The
# last one — this suite's whole log, minus every frame before it — is the
# only one a "TIME held still" comparison should ever read from: an earlier
# frame is a stale answer, not proof of anything happening now.
# `python3`, not `awk`: the marker's own `[` and `?` read as an unescaped
# bracket expression and quantifier the moment they reach a regex engine,
# which is exactly what a multi-byte `RS` is to (g)awk — a plain byte split
# sidesteps that rather than fighting it.
_last_frame() {
  python3 - "$BOARD_LOG" <<'PY'
import sys
data = open(sys.argv[1], "rb").read()
sys.stdout.buffer.write(data.split(b"\x1b[?2026h\x1b[H")[-1])
PY
}

# `dim_nothing` (src/status/view.rs) colours only the dash a figure has
# none — a real TIME, OUT or COST reads as plain text — but the state dot
# and the row's own colour still wrap the rest of the line in SGR escapes,
# which this strips so a plain grep can read what is left.
_strip_ansi() { sed -E $'s/\x1b\\[[0-9;]*[a-zA-Z]//g'; }

# `task`'s own TIME column, off the board's last frame: the one
# duration-shaped token — `12s`, `1m 02s`, `1h 05m`, `human_secs`'s three
# shapes — on the row naming it. Empty when the row shows a dash instead of
# a figure (nothing banked yet), which a caller comparing two reads of this
# treats as "still nothing" rather than a match worth trusting.
#
# The group's own header line, marked `▌`, is skipped: `queue_hang` names a
# task's group after the task, so that header names it too, carries no
# TIME, and sits above the row this wants.
_row_time() {
  local task=$1
  _last_frame | _strip_ansi | grep -F "$task" | grep -vF '▌' | head -1 |
    grep -Eo '[0-9]+h [0-9]{2}m|[0-9]+m [0-9]{2}s|[0-9]+s' | tail -1
}

# ------------------------------- pause from the menu over a live agent lane
# The cursor's own panel: it names the step, calls it an agent turn, and lists
# its two answers under the text — `pause now` and `pause when <step>
# finishes` — with the cursor on the first.
#
# Turned on here, for the rest of the suite: a stand-in writes no transcript
# by default (agents/transcript.sh), so with nothing else `record_usage`
# would have nothing to harvest and every lane from here on would bank
# nothing at all — exactly what the TIME-unchanged check below needs a real,
# non-dash figure to hold still. Same incantation disaster.sh uses.
echo 4000 > "$CTL/transcript"
queue_hang mid-turn
board_start
LANE_PID=$(lane_pid "mid-turn · implement" 30)
if [ -n "$LANE_PID" ]; then ok "the lane is really mid-turn"
else bad "the lane is really mid-turn"; fi
draws "the board draws the run" "mid-turn"

# The cursor starts on the first row on its own now — mid-turn is the only
# row here — so the menu reaches it with no `down` needed first. It lists
# open, pause and restart for a running row, with the cursor on open.
press o
draws "\`o\` over a live lane opens a menu titled with the task" "╭─ mid-turn ─"
draws "listing open first, under the cursor" "> open"
draws "then pause" "  pause"
draws "then restart" "  restart"
draws "under the shared key row" "[enter] confirm   [esc] close"
stage_stays "the task file is untouched while the menu is open" mid-turn implement
press $'\x1b'
stops_drawing "esc closes the menu" "╭─ mid-turn ─"

# ------------------------------------------- open with a missing editor
# An `editor.toml` naming a program that is not on `PATH` refuses the open,
# and the board shows the refusal, remedy and all, in a popup closed with
# `enter` alone. Under `$HOME`, which fixture.sh points at a scratch folder.
mkdir -p "$HOME/.spoolway"
printf '[editor]\ncommand = "notepd.exe"\n' > "$HOME/.spoolway/editor.toml"
OPEN_MARK=$(wc -l < "$BOARD_LOG")
pick_item open
draws_since_waited "a refused open shows a popup titled open task" \
  "╭─ open task ─" "$OPEN_MARK"
draws_since "naming the program that is not on PATH" \
  "\`notepd.exe\` is not on PATH" "$OPEN_MARK"
draws_since "and how to fix it" "~/.spoolway/editor.toml" "$OPEN_MARK"
# The key row is read only off the frame on screen with the popup up. The
# options menu `pick_item` opened after OPEN_MARK draws `[enter] confirm
# [esc] close` itself, so a check reaching back that far would pass for a
# popup with an `esc` key or no key row at all.
POPUP_FRAME=$(frame_showing "╭─ open task ─")
if grep -qF -- "[enter] confirm" <<<"$POPUP_FRAME"; then ok "closed with enter"
else bad "closed with enter (the popup on screen has no \"[enter] confirm\")"; sed 's/^/        /' <<<"$POPUP_FRAME"; fi
if grep -qF -- "[esc] close" <<<"$POPUP_FRAME"; then
  bad "and enter alone (the board drew \"[esc] close\")"; sed 's/^/        /' <<<"$POPUP_FRAME"
else ok "and enter alone"; fi
# Read only off frames drawn after `esc`, so this also proves the board
# keeps redrawing under the popup.
ESC_MARK=$(wc -l < "$BOARD_LOG")
press $'\x1b'
settle_frames 2
draws_since "esc leaves the popup up, on a board still redrawing" \
  "╭─ open task ─" "$ESC_MARK"
if kill -0 "$LANE_PID" 2>/dev/null; then ok "and the lane runs on under it"
else bad "and the lane runs on under it"; fi
press $'\r'
stops_drawing "enter closes the popup" "╭─ open task ─"
stage_stays "and the task is where it was" mid-turn implement
rm "$HOME/.spoolway/editor.toml"

pick_item pause

draws "picking pause over a live lane opens a panel naming the task" "pause mid-turn"
draws "the panel names the step and calls it an agent turn" "implement    agent"
draws "and says pausing aborts it" "Pausing aborts the step it is on:"
draws "and lists pause now first, under the cursor" "> pause now"
draws "with the scheduled pause as the second answer" "pause when implement finishes"
draws "and the one key row every popup shares" "[enter] confirm   [esc] close"
stage_stays "the task file is untouched while the panel is open" mid-turn implement
if kill -0 "$LANE_PID" 2>/dev/null; then ok "and the lane is still running"
else bad "and the lane is still running"; fi

# A letter does not answer anything — not `s`, which once scheduled — so
# it leaves the panel open and acts on nothing. Keys are read in the order
# they are sent, so the panel's own cursor moving down and back after them is
# proof both letters have been read, and the task file can be read for what
# they did rather than for what they have not got round to yet.
press x
press s
press $'\x1b[B'
shows "> pause when implement finishes" 10
press $'\x1b[A'
shows "> pause now" 10
stage_stays "a stray key over the panel changes nothing" mid-turn implement
if grep -qF "gate_at:" "$SPOOLWAY_PROJECT_HOME/queue/mid-turn.md"; then
  bad "and \`s\` no longer schedules a pause"
else ok "and \`s\` no longer schedules a pause"; fi
draws_now "and the panel is still up" "╭─ pause mid-turn"

press $'\x1b'
stops_drawing "esc closes the panel" "pause mid-turn"
stage_stays "and leaves the task where it was" mid-turn implement
if kill -0 "$LANE_PID" 2>/dev/null; then ok "and leaves the lane running"
else bad "and leaves the lane running"; fi

# The second answer leaves the turn running and writes a schedule instead of
# an interrupt — the NEXT column carries it, and the task's own file gains
# `gate_at`.
pick_item pause
shows "╭─ pause mid-turn"
press $'\x1b[B'
press $'\r'
stops_drawing "the second answer closes the panel without aborting anything" "pause mid-turn"
stage_stays "the task stays right where it was" mid-turn implement
has "and gains a schedule naming its own step" "gate_at: implement" \
  "$SPOOLWAY_PROJECT_HOME/queue/mid-turn.md"
draws "and the NEXT column says where it is headed" "paused after implement"
if kill -0 "$LANE_PID" 2>/dev/null; then ok "and the lane is still running"
else bad "and the lane is still running"; fi

# Answering it again, over a fresh panel on the same still-live step, clears
# the schedule it just wrote: the answer now reads `cancel the scheduled
# pause`.
pick_item pause
shows "╭─ pause mid-turn"
draws "the second answer reads as a cancel while a schedule stands" \
  "cancel the scheduled pause"
press $'\x1b[B'
press $'\r'
stops_drawing "answering it again closes the panel too" "pause mid-turn"
lacks "and clears the schedule it named" "gate_at:" \
  "$SPOOLWAY_PROJECT_HOME/queue/mid-turn.md"

pick_item pause
shows "╭─ pause mid-turn"
press $'\r'
stage_reaches "enter parks the task" mid-turn paused 25
if poll_while 15 kill -0 "$LANE_PID"; then ok "and the turn it named is over"
else bad "and the turn it named is over"; fi

# A paused row's TIME is the busy time it already banked, not the wait for
# a person to come back, and it must hold still while the task stays paused
# (acceptance criterion 5, gh-378 / issue #380).
#
# This suite runs the headless backend (`configure_project`), which does not
# keep a lane resident while it waits, so `hold_for_block` never runs here.
# The line the row reads is banked by the headless `interrupt_lane` through
# `usage::bank_lane`, whose `wall_s` is always 0 — so the figure held still
# is `0s`, not the seconds `mid-turn` actually ran. That bank lands whenever
# the interrupt does rather than on a dispatcher pass, so this polls for a
# figure rather than reading one `next_frame` after the pause and mistaking
# a bank that has not landed yet for a dash that never will.
_row_time_is_set() { [ -n "$(_row_time "$1")" ]; }
if poll_until 30 _row_time_is_set mid-turn; then
  PAUSED_TIME=$(_row_time mid-turn)
  ok "the paused row shows a real TIME ($PAUSED_TIME)"
else
  PAUSED_TIME=""
  bad "the paused row never shows a TIME at all"
  tail -30 "$BOARD_LOG" | sed 's/^/        /'
fi
settle_frames 15
STILL_TIME=$(_row_time mid-turn)
if [ "$STILL_TIME" = "$PAUSED_TIME" ]; then ok "and TIME is still $PAUSED_TIME after a wait, unchanged"
else
  bad "TIME moved while paused: was $PAUSED_TIME, now $STILL_TIME"
  tail -30 "$BOARD_LOG" | sed 's/^/        /'
fi

# ------------------- stopping from the stop popup over a live lane
# `enter` over a running dispatcher asks how to stop it: `let running steps
# finish` waits, `interrupt them now` interrupts them. That answer parks each task it
# interrupted with a mark saying the stop parked it, and leaves everything
# else — here `behind`, which has nothing running — exactly where it was.
# Starting again resumes exactly the marked tasks.
queue_hang busy
queue_idle behind busy
BUSY_PID=$(lane_pid "busy · implement" 30)
if [ -n "$BUSY_PID" ]; then ok "a second lane is mid-turn"
else bad "a second lane is mid-turn"; fi

press $'\r'
draws "\`enter\` over a running dispatcher opens the stop popup" "╭─ stop dispatching"
draws "saying no new steps will start" "No new steps will be started."
draws "offering to let running steps finish first" "> let running steps finish"
draws "and to interrupt them" "interrupt them now"
draws "under the shared key row" "[enter] confirm   [esc] close"
stage_stays "no task file is written while the popup is open" busy implement

press $'\x1b'
stops_drawing "esc closes the stop popup" "No new steps will be started."
MARK=$(wc -l < "$BOARD_LOG")
draws_since_waited "and the dispatcher is still running" "dispatcher running · pid " "$MARK"
if kill -0 "$BUSY_PID" 2>/dev/null; then ok "and leaves the lane running"
else bad "and leaves the lane running"; fi

MARK=$(wc -l < "$BOARD_LOG")
press $'\r'
draws_since_waited "\`enter\` asks again" "No new steps will be started." "$MARK"
MARK=$(wc -l < "$BOARD_LOG")
# `i` first, then the popup's own `↓`: once the screen shows the cursor moved,
# `i` has been read too, so the task file says what it did.
press i
press $'\x1b[B'
shows "> interrupt them now" 10
stage_stays "\`i\` no longer answers the stop popup" busy implement
press $'\r'
stage_reaches "interrupt them now parks the task it interrupted" busy paused 25
if poll_while 15 kill -0 "$BUSY_PID"; then ok "and the turn it interrupted is over"
else bad "and the turn it interrupted is over"; fi
has "marking the park as the stop's" "parked_by_stop: true" \
  "$SPOOLWAY_PROJECT_HOME/queue/busy.md"
has "beside the step it came off" "parked_from: implement" \
  "$SPOOLWAY_PROJECT_HOME/queue/busy.md"
draws_since_waited "and the dispatcher stops" "dispatcher stopped" "$MARK" 30
draws "the row says the next start resumes it" \
  "→ implement — resumes when dispatching starts" 30
stage_stays "a task with nothing running is left alone" behind queued
stage_stays "and so is one a person paused" mid-turn paused

# Starting again, from the same tab, resumes the one task the stop parked.
screen_dispatch
stage_reaches "starting again resumes what the stop interrupted" busy implement 25
lacks "spending the stop's mark" "parked_by_stop:" \
  "$SPOOLWAY_PROJECT_HOME/queue/busy.md"
RESUMED_PID=$(lane_pid "busy · implement" 30)
if [ -n "$RESUMED_PID" ] && [ "$RESUMED_PID" != "$BUSY_PID" ]; then
  ok "in a lane of its own again"
else bad "in a lane of its own again"; fi
stage_stays "the task a person paused stays paused" mid-turn paused

# The rest of this suite wants `busy` stopped, and its dependents parked off
# `queued` for `R` to reach below. `P` used to do that in one key; with it
# gone, each goes through `spoolway queue pause`, the same `park` the board's
# pause item writes.
must "pausing busy by hand" "$SPOOLWAY" queue pause busy
stage_reaches "busy parks" busy paused 25
lacks "and a park by hand carries no stop mark" "parked_by_stop:" \
  "$SPOOLWAY_PROJECT_HOME/queue/busy.md"
must "pausing behind by hand" "$SPOOLWAY" queue pause behind
queue_idle late behind
must "pausing late by hand" "$SPOOLWAY" queue pause late

# ----------------------------------------------- a paused row is a no-op
MARK=$(wc -l < "$BOARD_LOG")
press o
shows "│  > open " 10
draws_since "the menu on an already-paused row opens" "> open" "$MARK"
draws_since "and offers restart" "  restart" "$MARK"
never_draws "but offers no pause to pick" "    pause    " "$MARK"
press $'\x1b'
poll_while 10 _now_says "│  > open "

# ------------------------------- pausing a task that never started at all
# Nothing live, nothing declared for `queued` in any pipeline, so this parks
# on the spot — the one thing worth checking here is the record it leaves.
queue_idle never-run late
must "pausing a task that never started" "$SPOOLWAY" queue pause never-run
stage_reaches "pausing a task that never started parks it at once" never-run paused 25
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

# --------------------------------------------- `U` is no longer a board key
# Unqueueing everything unstarted is `spoolway queue unqueue --all` now. Two
# tasks sit on `queued`: `stalled`, and `never-run` back where its resume put
# it. `U` on the board opens nothing and writes nothing; the command still
# carries both back to pending.
queue_idle stalled never-run
MARK=$(wc -l < "$BOARD_LOG")
# `U`, then `o`, whose menu is drawn only once every key before it has been
# read: a panel `U` opened would be on screen by then, and in the log since
# the mark. The menu is closed again before anything else is typed.
press U
press o
shows "│  > open " 10
never_draws "\`U\` opens no unqueue-all panel" "tasks have not started:" "$MARK"
press $'\x1b'
poll_while 10 _now_says "│  > open "
stage_stays "and writes nothing" stalled queued
stage_stays "to any queued task" never-run queued

_gone() { [ ! -e "$SPOOLWAY_PROJECT_HOME/queue/$1.md" ]; }
must "unqueue --all carries out what U used to" "$SPOOLWAY" queue unqueue --all
if poll_until 25 _gone stalled; then ok "the task leaves the queue"
else bad "the task leaves the queue"; tail -30 "$BOARD_LOG" | sed 's/^/        /'; fi
has "the task lands back in pending" "id: stalled" "$SPOOLWAY_PROJECT_HOME/pending/stalled.md"
has "and so does the one that never ran" "id: never-run" "$SPOOLWAY_PROJECT_HOME/pending/never-run.md"

# --------------------------- unqueue from the menu carries a dependent chain
# The reach this task adds: unqueue on a task a still-queued task depends on no
# longer refuses outright — it lists the whole chain of unstarted tasks that
# reach it through `depends_on` and carries every one of them back to
# pending together. Restarted fresh, with its own group sorted ahead of
# every other row's, so the freshly restarted board's own cursor
# opens on it directly — the same trick the blocked-row section below uses
# for the same reason.
#
# `chain-head` depends on `chain-gate`, a `hang`-mode lane of its own — only
# a group's first task may name another group, and only that group's last
# task, so gating `chain-head` on something still paused elsewhere in the
# run, the way `queue_idle` does for every other idle task here, is not
# available inside a fresh group of its own. A live lane that never returns is: with nothing
# to make it `Done`, `chain-head` never becomes ready, and is still
# genuinely `queued` — not already off running its own pipeline — when unqueue
# is picked. The cursor already opens on `chain-gate`, the first row of its
# own fresh group; `chain-gate` sorts above `chain-head` within it, so
# `cursor_to` takes one `down` to reach the row this section picks on.
board_stop
mkdir -p "$CTL"
echo hang > "$CTL/chain-gate"
task_doc "$LIVE/chain-gate.md" chain-gate "$BODY" "group: 0-chain"
must "chain-gate queues" "$SPOOLWAY" queue add --from "$LIVE/chain-gate.md"
task_doc "$LIVE/chain-head.md" chain-head "$BODY" "group: 0-chain" \
  "depends_on: [chain-gate]"
must "chain-head queues" "$SPOOLWAY" queue add --from "$LIVE/chain-head.md"
task_doc "$LIVE/chain-tail.md" chain-tail "$BODY" "group: 0-chain" \
  "depends_on: [chain-head]"
must "chain-tail queues" "$SPOOLWAY" queue add --from "$LIVE/chain-tail.md"
board_start

CHAIN_GATE_PID=$(lane_pid "chain-gate · implement" 30)
if [ -n "$CHAIN_GATE_PID" ]; then ok "the gate lane is really mid-turn"
else bad "the gate lane is really mid-turn"; fi
draws "the board draws the chain" "chain-tail"

cursor_to chain-head
pick_item unqueue

draws "unqueue on the chain's head opens a panel naming both" "unqueue chain-head"
draws "the dependent, marked with what it depends on" \
  "chain-tail   (depends on chain-head)"
draws "and offers one answer for both" "> unqueue them"
draws "under the shared key row" "[enter] confirm   [esc] close"
stage_stays "nothing moves while the panel is open" chain-head queued
stage_stays "not even the dependent" chain-tail queued

press $'\r'
if poll_until 25 _gone chain-head; then ok "enter carries the head back to pending"
else bad "enter carries the head back to pending"; tail -30 "$BOARD_LOG" | sed 's/^/        /'; fi
if poll_until 25 _gone chain-tail; then ok "and carries the dependent along with it"
else bad "and carries the dependent along with it"; tail -30 "$BOARD_LOG" | sed 's/^/        /'; fi
has "the head's task lands back in pending" "id: chain-head" \
  "$SPOOLWAY_PROJECT_HOME/pending/chain-head.md"
has "and so does the dependent's" "id: chain-tail" \
  "$SPOOLWAY_PROJECT_HOME/pending/chain-tail.md"

# ------------------------- pause reaches a blocked row with a live lane
#
# The reach this task adds: the unblocker can be mid-turn on a `blocked` row
# exactly as an implementer can be mid-turn on `implement`, once the run is
# unattended, and pause interrupts that turn the same way. This needs a
# restart — `unattended.enabled` is fixed in the lock at launch and
# `blocked_agent` is baked into the pipelines when they load — so this task
# is queued straight into the live queue directory, on `blocked` already,
# with `blocked_from` set the way a real block leaves it; `routines.sh` and
# `trials.sh` write straight into the live queue directory with `task_doc`
# the same way, for a fixture no dispatcher needs to walk there itself. Its
# group sorts ahead of every other row's, so the freshly restarted
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
  "stage: blocked" "blocked_from: implement" "group: 0-blocked"
board_start

STUCK_PID=$(lane_pid "stuck · blocked" 30)
if [ -n "$STUCK_PID" ]; then ok "the unblocker is really mid-turn on the blocked row"
else bad "the unblocker is really mid-turn on the blocked row"; fi
draws "the board draws the staffed blocked row" "stuck"

# While the unblocker is mid-turn, a resume from the command line would start
# a second lane in the same worktree and lose its verdict, so both commands
# refuse and name the pause that takes the task from it.
STUCK_FILE="$SPOOLWAY_PROJECT_HOME/queue/stuck.md"
STUCK_BEFORE=$(cat "$STUCK_FILE")
refuses "\`spoolway resume\` refuses a blocked task whose unblocker is live" \
  "queue pause stuck" "$SPOOLWAY" resume stuck
refuses "\`spoolway queue resume\` refuses it too" \
  "queue pause stuck" "$SPOOLWAY" queue resume stuck
if [ "$(cat "$STUCK_FILE")" = "$STUCK_BEFORE" ]; then ok "the refused resumes left the task file alone"
else bad "the refused resumes left the task file alone"; fi

cursor_to stuck
pick_item pause

draws "pause over the blocked row opens a panel naming the task" "pause stuck"
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
# `a_stop_interrupt_ends_a_live_agent_turn_and_the_next_start_resumes_it`
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

# --------------------------------------- restart ends the lane it finds
# The unblocker the resume above put back is genuinely mid-turn on `blocked`
# again, which is the live lane a restart has to end. `spoolway restart`
# writes the restart to the task, then tears down every lane the task owns.
# The step it starts over is the one the block stopped, `implement`, and the
# new lane the dispatcher starts there has a name of its own, so the old
# process is what shows the teardown.
STUCK_PID=$(lane_pid "stuck · blocked" 30)
if [ -n "$STUCK_PID" ]; then ok "the unblocker is mid-turn again before the restart"
else bad "the unblocker is mid-turn again before the restart"; fi
# The pid lands before the launching pass writes the task file, and a restart
# that read the task in that gap is refused as a report that landed first —
# rightly, since the file changed under it. `lanes.json` is written at the end
# of that same pass, so once the lane is on record the task file is settled.
poll_until 15 lane_on_record "stuck · blocked"

RESTART_OUT=$("$SPOOLWAY" restart stuck -m "e2e restart" 2>&1)
if [ $? -eq 0 ]; then ok "restarting the blocked task"
else bad "restarting the blocked task"; sed 's/^/        /' <<<"$RESTART_OUT"; fi
for want in "tore down lane \`stuck · blocked\`" "stuck: -> implement (fresh session)"; do
  if grep -qF "$want" <<<"$RESTART_OUT"; then ok "it prints: $want"
  else bad "it prints: $want"; sed 's/^/        /' <<<"$RESTART_OUT"; fi
done
if [ -n "$STUCK_PID" ] && poll_while 15 kill -0 "$STUCK_PID"; then
  ok "and the lane's process is gone"
else bad "and the lane's process is gone"; fi
has "the status log records the note" "e2e restart" \
  "$SPOOLWAY_PROJECT_HOME/queue/stuck.md"
stage_reaches "the task is sent back to the step the block stopped" stuck implement 25
# The restarted `implement` lane hangs too, as `stuck` is still set to. Parked
# here so it does not hold a slot for the sections below — but only once the
# dispatcher has launched it. The restart writes the stage at once, and a park
# before the launch would leave `implement` with no lane ever run, and the
# board's own restart below with no earlier session to compare against.
STUCK_LOG="$SPOOLWAY_PROJECT_HOME/headless/logs/stuck · implement.log"
last_session() { grep -ao -- '--session-id [^ ]*' "$STUCK_LOG" 2>/dev/null | tail -1 | awk '{print $2}'; }
launches() { grep -ac 'lane argv:' "$STUCK_LOG" 2>/dev/null || true; }
_launched_past() { [ "$(launches)" -gt "$1" ] 2>/dev/null; }
if poll_until 30 _launched_past 0; then ok "the dispatcher launches the restarted step"
else bad "the dispatcher launches the restarted step"; fi
must "the restarted lane is parked so it holds no slot" "$SPOOLWAY" queue pause stuck

# ------------------------------ restart picked from the menu on the board
# The same restart, reached from the board: restart in `stuck`'s menu opens
# the restart panel, and `enter` inside it carries the restart out. `stuck` is
# parked off `implement` now, a row a restart accepts, and the lane that ran
# there opened a session of its own. The stand-in echoes its argv into the
# lane's log, which is appended to across launches, and `pi` is pinned with
# `--session-id`: a continued conversation passes the same id again, and a
# fresh one a new id. So the last id in the log, before and after, is the
# proof the step came back on a new session rather than its old one.
#
# The cursor is walked onto `stuck`'s own row — found by the board's cursor
# mark, not by opening every row's menu on the way — and restart is taken off
# its menu by name. The panel's text is read off the frame on screen with the
# panel up.
#
# The restart panel has one answer, so no key short of `enter` and `esc`
# shows anything for having been read. A stray `s` is proven read by the
# `esc` behind it closing the panel — keys are read in the order they are
# sent — and the panel is opened again for the `enter` that restarts.
OLD_SESSION=$(last_session)
OLD_LAUNCHES=$(launches)
if [ -n "$OLD_SESSION" ]; then ok "the parked lane's session is in its log before the restart"
else bad "the parked lane's session is in its log before the restart"; fi

SAW_RESTART=""
if cursor_to stuck && pick_item restart \
  && RESTART_FRAME=$(frame_showing "╭─ restart stuck ─" 10); then
  SAW_RESTART=stuck
  for want in "╭─ restart stuck ─" "step      implement" "lane      stuck · implement" \
    "is briefed from" "> restart" "[enter] confirm   [esc] close"; do
    if grep -qF -- "$want" <<<"$RESTART_FRAME"; then ok "the restart panel says: $want"
    else bad "the restart panel says: $want"; sed 's/^/        /' <<<"$RESTART_FRAME"; fi
  done
  press s
  press $'\x1b'
  poll_while 10 _now_says "╭─ restart stuck ─"
  stage_stays "a stray \`s\` over the restart panel restarts nothing" stuck paused
  pick_item restart
  shows "╭─ restart stuck ─" 10
  press $'\r'
fi
if [ "$SAW_RESTART" = stuck ]; then ok "restart in the parked row's menu opens the restart panel"
else bad "restart in the parked row's menu opens the restart panel"; tail -30 "$BOARD_LOG" | sed 's/^/        /'; fi
stage_reaches "enter on the panel's restart sends the task back to the step it held" stuck implement 25
if poll_until 30 _launched_past "${OLD_LAUNCHES:-0}"; then ok "the dispatcher launches the step again"
else bad "the dispatcher launches the step again"; fi
NEW_SESSION=$(last_session)
if [ -n "$NEW_SESSION" ] && [ "$NEW_SESSION" != "$OLD_SESSION" ]; then
  ok "on a new session, not the one it held"
else bad "on a new session, not the one it held (before $OLD_SESSION, after $NEW_SESSION)"; fi
# Anchored to the front matter: the Status Log's own notes say "restart" too.
_restart_spent() { ! grep -q '^restart:' "$SPOOLWAY_PROJECT_HOME/queue/stuck.md"; }
if poll_until 15 _restart_spent; then ok "and the restart key is spent by that launch"
else bad "and the restart key is spent by that launch"; fi
# Hanging again, as `stuck` is set to: parked so it holds no slot below.
must "the board-restarted lane is parked so it holds no slot" "$SPOOLWAY" queue pause stuck

# ------------------------ restart picked from a command step's menu
# The same panel over a task standing on a command step, which has no lane
# and no session: it names the step and the run instead, and `enter` ends the
# command's process and runs the step again from the start. `build` records
# each run's shell pid in a file of its own, so a second line is a second run,
# and waits for a release file, so the task finishes once this section is done
# with it rather than holding a running command into the sections below. The
# pipeline is written while the board is down, since a running dispatcher
# keeps the pipelines it started with.
board_stop
BUILD_RUNS="$LIVE/build.runs"
BUILD_RELEASE="$LIVE/build.release"
rm -f "$BUILD_RUNS" "$BUILD_RELEASE"
cat > .spoolway/pipelines/board-build.yml <<YML
steps:
  - id: build
    run: echo \$\$ >> "$BUILD_RUNS"; for _ in \$(seq 1 3000); do [ -e "$BUILD_RELEASE" ] && exit 0; sleep 0.1; done
    on_pass: done
YML
task_doc "$LIVE/build-cmd.md" build-cmd "$BODY" "group: build-cmd" \
  "pipeline: board-build"
must "build-cmd queues" "$SPOOLWAY" queue add --from "$LIVE/build-cmd.md"
board_start

if poll_until 60 test -s "$BUILD_RUNS"; then ok "the command step is running"
else bad "the command step is running"; fi
BUILD_FIRST=$(head -n1 "$BUILD_RUNS" 2>/dev/null)

# Reached the way the section above reaches `stuck`. `build-cmd` is running,
# so its menu is open, pause and restart.
SAW_RESTART=""
if cursor_to build-cmd && pick_item restart \
  && RESTART_FRAME=$(frame_showing "╭─ restart build-cmd ─" 10); then
  SAW_RESTART=build-cmd
  for want in "╭─ restart build-cmd ─" "step      build" "run       command · " \
    "The command's process is ended and \`build\` runs again" \
    "from the start. Its log is kept as the previous log." "> restart" \
    "[enter] confirm   [esc] close"; do
    if grep -qF -- "$want" <<<"$RESTART_FRAME"; then ok "the command form says: $want"
    else bad "the command form says: $want"; sed 's/^/        /' <<<"$RESTART_FRAME"; fi
  done
  for unwanted in "lane      build-cmd" "session   "; do
    if grep -qF -- "$unwanted" <<<"$RESTART_FRAME"; then
      bad "the command form names no $unwanted"; sed 's/^/        /' <<<"$RESTART_FRAME"
    else ok "the command form names no $unwanted"; fi
  done
  press $'\r'
fi
if [ "$SAW_RESTART" = build-cmd ]; then ok "restart in a command step's menu opens the command form"
else bad "restart in a command step's menu opens the command form"; tail -30 "$BOARD_LOG" | sed 's/^/        /'; fi
build_ran_again() { [ "$(wc -l < "$BUILD_RUNS")" -ge 2 ]; }
if poll_until 60 build_ran_again; then ok "enter on the panel runs the command again from the start"
else bad "enter on the panel runs the command again from the start"; fi
if [ -n "$BUILD_FIRST" ] && ! kill -0 "$BUILD_FIRST" 2>/dev/null; then
  ok "and the run that was going is gone"
else bad "and the run that was going is gone (pid $BUILD_FIRST)"; fi
lacks "and no conversation is named for a command" "restart:" \
  "$SPOOLWAY_PROJECT_HOME/queue/build-cmd.md"
touch "$BUILD_RELEASE"
if poll_until 60 _gone build-cmd; then ok "the released command finishes its task"
else bad "the released command finishes its task"; fi

# ------------------------- a schedule catches a failing step, not only a pass
# The pause panel above already proved it writes and clears `gate_at`; what a schedule
# does once the step it names actually fails, rather than passes, is
# `commands::report`'s own road, not a keypress this suite can watch — so
# this drives it with a canned patch that cannot apply, the mock's own way of
# making `implement` report `--fail` for real rather than the marker-file
# work every other task above got.
mkdir -p "$SOLUTIONS/pause-fail-catch"
printf 'not a real patch\n' > "$SOLUTIONS/pause-fail-catch/implement.patch"
task_doc "$LIVE/pause-fail-catch.md" pause-fail-catch "$BODY" "group: pause-fail-catch" \
  "gate_at: implement"
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

# ------------------- a schedule on `blocked` holds the unblocker's own pass
# `unattended.blocked_agent` is still `pi` here, whose default turn reports a
# pass. The task is seeded stopped on `blocked` the way a real block leaves
# it, with the schedule on `blocked` itself. `blocked` declares no `on_pass`,
# so the held pass has to go where it would have gone unheld: the `on_pass` of
# `review`, the step it stopped at (not the entry, whose `on_pass` is a
# different step), and not back onto `blocked` for a second unblocker lane
# whose verdict would be thrown away.
task_doc "$SPOOLWAY_PROJECT_HOME/queue/hold-unblock.md" hold-unblock "$BODY" \
  "stage: blocked" "blocked_from: review" "gate_at: blocked" "group: hold-unblock"
stage_reaches "the unblocker's pass is held on paused" hold-unblock paused 30
has "at blocked, the step whose pass it caught" "paused_at: blocked" \
  "$SPOOLWAY_PROJECT_HOME/queue/hold-unblock.md"
draws "the board's NEXT column names review's on_pass, not blocked" "[r] → document" 30
must "resuming it carries the pass on" "$SPOOLWAY" resume hold-unblock
stage_reaches "and it lands on review's on_pass, with no second unblocker" \
  hold-unblock document 25

# --------------------------- a gated stop offers a key before every command
# The mockup this task built: a report that lands a task on `paused` names
# what it offers key first, then the command — never a bare key with nothing
# to run, and never a command with no key in front of it. The board's own
# row for the same stop names the key and the step it resumes to, and no
# command: on the board, `r` is the way out. Then `spoolway task edit`,
# run the way a person — or the lane in that pane, once it is stopped —
# would run it: from outside the lane entirely, against a task already
# parked.
task_doc "$LIVE/gate-edit.md" gate-edit "$BODY" "group: gate-edit" \
  "gate_at: implement"
must "gate-edit queues" "$SPOOLWAY" queue add --from "$LIVE/gate-edit.md"

stage_reaches "a gated pass parks on paused" gate-edit paused 30
draws "the board's row offers the key and the step it resumes to" \
  "[r] → review" 30

MOCKUP="$LIVE/mockup-section.txt"
printf 'held here for a person, edited from outside the lane\n' > "$MOCKUP"
must "\`task edit\` rewrites the stopped task's own section" \
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

# ------------------------------ `r` sends a queued park back to `queued`
# The reach this task adds: `r` on a row parked off `queued` itself resumes
# it at once, with no picker, and does not hold it for a dependency the way
# a real step's row still would. `behind` and `late` have sat on `paused`
# since `queue pause` parked them off `queued`, both still gated by `busy`
# — `behind` directly, `late` through `behind` — and neither has a finished
# dependency. `gate-edit` is still paused too, a real gate, so `r` there
# opens the step picker with the cursor on the step a plain resume goes to,
# and `enter` takes that road.
#
# `r` is pressed on those three rows and nowhere else, each reached by
# `cursor_to`, and every key waits for the screen to show the one before it
# landed. This used to walk the cursor down from the top pressing `r` on
# every row and pacing each key on "a frame was drawn since", which a frame
# already on its way satisfied as readily as the one the key caused: a `↓`
# sent before a picker had opened then moved the picker's cursor off its
# preselected row instead of moving the board's.
#
# What `enter` did is read off the gate's own record, not off `stage:`
# alone. Once resumed, `gate-edit` runs on through `review` and `document` to
# `handover`, which fails here for want of a forge, and the unattended
# unblocker sends it back there until the run gives up and parks it on
# `paused` again — several passes, but no person in the loop, so the whole
# round trip can be over before anything looks. Watching for "not `paused`"
# then waited its full bound on a task that had already left and come back,
# and failed. The Status Log line the gate's release writes stays put
# whatever the task does next, and names the step the preselected row did.
SAW_PICKER=""
cursor_to late && press r
cursor_to behind && press r
if cursor_to gate-edit; then
  press r
  if shows "╭─ resume gate-edit ─" 10; then SAW_PICKER=gate-edit; fi
fi
if [ "$SAW_PICKER" = gate-edit ]; then ok "\`r\` on the real gate opens the step picker"
else bad "\`r\` on the real gate opens the step picker"; _frame_now | sed 's/^/        /'; fi
if [ "$SAW_PICKER" = gate-edit ] && poll_until 10 _now_matches '│  > [a-z-]+ .*\(next\)'; then
  press $'\r'
fi
stage_reaches "a row parked off \`queued\` goes back to \`queued\`, dependency or not" \
  late queued 25
stage_reaches "and every other queued park along with it" behind queued 25
_gate_released() {
  grep -qF -- "→ \`review\`: \`implement\` released at the gate" \
    "$SPOOLWAY_PROJECT_HOME/queue/gate-edit.md" "$SPOOLWAY_PROJECT_HOME/archive/gate-edit.md" 2>/dev/null
}
if poll_until 25 _gate_released; then ok "\`enter\` on the picker's preselected row resumes the gate"
else
  bad "\`enter\` on the picker's preselected row resumes the gate (gate-edit is at $(stage_of gate-edit))"
  sed -n '/^## Status Log/,$p' "$SPOOLWAY_PROJECT_HOME/queue/gate-edit.md" 2>/dev/null | sed 's/^/        /'
  _frame_now | sed 's/^/        /'
fi
lacks "carrying no leftover \`parked_from\`" "parked_from:" \
  "$SPOOLWAY_PROJECT_HOME/queue/late.md"

# --------- interrupting from the stop popup reaches every live lane at once
# The stop popup's `interrupt them now` reaches every running step in the run,
# not only the one a cursor sits on: six live lanes, one answer, six parks,
# each marked as the stop's. The dispatcher keeps passing while the popup writes those
# parks, and a pass holding a copy of a task read before the park cannot
# overwrite it — `persist_task`'s own fingerprint check, proven at the unit
# level by `persist_task_does_not_overwrite_a_park_typed_mid_pass` in
# `src/dispatch.rs`; this is that park landing for real, over live lanes.
for n in 0 1 2 3 4 5; do
  queue_hang "pass-race-$n"
done
for n in 0 1 2 3 4 5; do
  if [ -n "$(lane_pid "pass-race-$n · implement" 30)" ]; then ok "pass-race-$n is mid-turn"
  else bad "pass-race-$n is mid-turn"; fi
done
MARK=$(wc -l < "$BOARD_LOG")
press $'\r'
draws_since_waited "\`enter\` opens the stop popup again" "No new steps will be started." "$MARK"
MARK=$(wc -l < "$BOARD_LOG")
press $'\x1b[B'
press $'\r'
for n in 0 1 2 3 4 5; do
  stage_reaches "interrupt them now parks every live lane" "pass-race-$n" paused 30
  has "each marked as the stop's" "parked_by_stop: true" \
    "$SPOOLWAY_PROJECT_HOME/queue/pass-race-$n.md"
done
draws_since_waited "and the dispatcher stops" "dispatcher stopped" "$MARK" 30

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
