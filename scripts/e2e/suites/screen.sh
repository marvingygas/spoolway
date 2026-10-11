#!/usr/bin/env bash
# Bare `spoolway`: the one screen with its dispatch, queue, routines, jobs
# and eval tabs, driven end to end as the whole binary. It opens only with stdout on a
# terminal, so the run is wrapped in `script`, which gives it a pty for stdout
# while the keys still arrive on an ordinary pipe — the same way every other
# screen suite scripts its keystrokes.
#
# What is asserted is the order of the frames it drew: the first is the queue
# tab, and `←` from there draws the dispatch tab's board, where `enter` starts
# a dispatcher and, with no step running, stops it at once with no popup.
# While one screen is open, a second `spoolway` or `spoolway dispatch` in
# the same project refuses. A refusal on the queue tab is drawn in a popup
# over the tab, and so is what a clean submission queued, ending on whether a
# dispatcher will pick it up. `→` from the queue tab reaches the routines tab.
# A full board in a short pane scrolls its task table under a marker row and
# keeps its key line. Off a terminal, bare `spoolway` still prints the grouped
# help.
#
# No `covers:` tag — the coverage map only enumerates `config.toml` keys and
# pipeline step keys, and a screen gesture is neither.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"
# shellcheck source=../fixture.sh
source "$HERE/../fixture.sh"
# shellcheck source=../agents.sh
source "$HERE/../agents.sh"

LIVE=${WORK:-$(mktemp -d)}

# Bare `spoolway` shows a newer release as a popup over the queue tab, and a
# popup takes every key — so a release cache that names one, or the lookup
# the first run here starts, would eat the keys every check below sends.
# What this suite drives is the screen, not the release check.
export SPOOLWAY_SKIP_VERSION_CHECK=1

new_repo "$LIVE/proj"
configure_project plan/live

# A fixture is never synced — every other command asks that only on a
# terminal — so the screen would open on its sync popup, which takes every
# key too. Brought up to date first, the same as `lib.sh`'s `on_screen` does.
must "the project synced" "$SPOOLWAY" sync

# One pending group, so the queue tab has a row of its own to draw.
BODY="$LIVE/body.md"
task_body "$BODY"
pending_doc cart-totals "$BODY" "group: cart"

# `script -qec` runs the pipeline under a pty and exits with its status. The
# pty is what makes stdout a terminal; stdin is the `printf` pipe, so the
# screen reads `←` and then runs out of keys, which ends it. The typescript
# `script` writes is everything the screen drew.
DRAWN="$LIVE/drawn.txt"
works "bare spoolway opens and ends when its keys run out" \
  script -qec "printf '\\033[D' | '$SPOOLWAY'" "$DRAWN"

# The real `TermGuard` the screen holds across its whole life enters the
# alternate screen and turns wheel-as-arrows off before drawing a thing, and
# reverses both on the way out — the one thing standing between a herdr pane
# and turning every cleared frame into scrollback (see platform.rs). `has`
# only checks that a string is somewhere in the file, so opening and closing
# are checked against the program's own output: `script`'s own "Script
# started"/"Script done" lines bookend it, one full line each, however `-q`
# is documented. Leaving first ends synchronized output (`ESC[?2026l`), so a
# terminal holding its paint for a frame cut off mid-write lets go of it
# before the screen is restored.
ENTER_ALT_SCREEN_AND_STOP_WHEEL=$'\033[?1049h\033[?1007l'
END_SYNC_RESTORE_WHEEL_AND_LEAVE_ALT_SCREEN=$'\033[?2026l\033[?1007h\033[?1049l'
PROGRAM_OUTPUT=$(sed -e '1d' -e '$d' "$DRAWN")
if [[ "$PROGRAM_OUTPUT" == "$ENTER_ALT_SCREEN_AND_STOP_WHEEL"* ]]; then
  ok "opens by entering the alternate screen and stopping the wheel"
else
  bad "opens by entering the alternate screen and stopping the wheel"
  head -c 40 "$DRAWN" | cat -v
fi
if [[ "$PROGRAM_OUTPUT" == *"$END_SYNC_RESTORE_WHEEL_AND_LEAVE_ALT_SCREEN" ]]; then
  ok "ends by ending synchronized output, restoring the wheel and leaving the alternate screen"
else
  bad "ends by ending synchronized output, restoring the wheel and leaving the alternate screen"
  tail -c 40 "$DRAWN" | cat -v
fi

# Every frame opens with the shared frame writer's own start code
# (`\x1b[?2026h\x1b[H` — see `src/screen/frame_writer.rs`), so the transcript
# splits into frames on it. The first one drawn is the queue tab's, its label
# bracketed on the strip. Colour codes are taken out of each frame — the
# strip's own bold and the colour of the tab drawn under it.
FIRST="$LIVE/first.txt"
LAST="$LIVE/last.txt"
awk 'BEGIN { RS = "\033\\[\\?2026h\033\\[H" } NR == 2 { print; exit }' "$DRAWN" |
  sed 's/\x1b\[[0-9;]*m//g' >"$FIRST"
awk 'BEGIN { RS = "\033\\[\\?2026h\033\\[H" } { last = $0 } END { print last }' "$DRAWN" |
  sed 's/\x1b\[[0-9;]*m//g' >"$LAST"

has "the strip names all five tabs in order" \
  "DISPATCH       [QUEUE]       ROUTINES        JOBS        EVAL" "$FIRST"
has "it opens on the queue tab" "─ groups ─" "$FIRST"
has "with the pending group listed" "cart" "$FIRST"
lacks "not on the dispatch tab" "dispatcher" "$FIRST"

has "← reaches the dispatch tab's board" "dispatcher stopped" "$LAST"
lacks "drawn inside a box with no title" "─ dispatch" "$LAST"
has "with dispatch the open tab on the strip" "← [DISPATCH]       QUEUE" "$LAST"
lacks "which is no longer the queue tab" "─ groups ─" "$LAST"

# `enter` on the dispatch tab starts a `spoolway dispatch` child and `enter`
# again, with no step running, stops it at once and draws no stop popup.
# This project's fake models give the warnings gate something to say, so the
# first `enter` opens the warnings popup first and `x` answers it — hiding it
# and starting the child. The keys are paced so the child has time to take
# the lock before the second `enter`, and time to go once asked; the pipe then
# runs out and the screen ends.
# Nothing is queued, which a child the screen started waits on rather than
# exiting — so a frame naming its pid while the queue is empty is also the
# proof it stayed up.
RUN="$LIVE/run.txt"
works "enter on the dispatch tab starts dispatching, and enter again stops it" \
  script -qec "{ printf '\\033[D\\r'; sleep 1; printf x; sleep 5; printf '\\r'; sleep 3; } | '$SPOOLWAY'" "$RUN"
sed 's/\x1b\[[0-9;]*m//g' "$RUN" >"$RUN.plain"
awk 'BEGIN { RS = "\033\\[\\?2026h\033\\[H" } { last = $0 } END { print last }' "$RUN" |
  sed 's/\x1b\[[0-9;]*m//g' >"$LAST"

has "enter asks the warnings gate first, as a popup" "─ before dispatching " "$RUN.plain"
has "whose keys read as drawn" "[enter] start dispatching   [esc] back   [x] hide until these change" "$RUN.plain"
has "the header names the child's pid while it runs" "dispatcher running · pid " "$RUN.plain"
has "and enter is offered to stop it" "[enter] stop dispatching" "$RUN.plain"
has "and it stays up on an empty queue" "Nothing queued" "$RUN.plain"
if grep -qaE "Good (morning|afternoon|evening)|Working late" "$RUN.plain"; then
  ok "the empty board greets the person"
else
  bad "the empty board greets the person (no greeting in $RUN.plain)"
fi
lacks "enter over it with no step running draws no stop popup" "─ stop dispatching " "$RUN.plain"
lacks "the start was not refused" "the dispatcher did not start" "$RUN.plain"
has "enter stops it at once" "dispatcher stopped" "$LAST"
has "and offers to start it again" "[enter] start dispatching" "$LAST"
lacks "with no popup about a stop it was asked for" "the dispatcher stopped" "$LAST"
LOCKFILE="$SPOOLWAY_PROJECT_HOME/dispatch.pid"
child_gone() {
  local p
  p=$(head -1 "$LOCKFILE" 2>/dev/null) || return 0
  [ -z "$p" ] || ! kill -0 "$p" 2>/dev/null
}
if poll_until 10 child_gone; then ok "the child is gone once the screen has ended"
else bad "the child is gone once the screen has ended"; fi

# The warnings gate checks the pipelines on disk when `enter` is pressed, not
# the copy the screen opened with. A pipeline written while the screen is up,
# whose prompt does not exist, is named by the popup; the earlier `x` hid only
# the findings that were there then, so the new one is not hidden by it.
LATE=".spoolway/pipelines/late-pipeline.yml"
RUN="$LIVE/late.txt"
works "a pipeline added after the screen opened is checked on enter" \
  script -qec "{ printf '\\033[D'; sleep 1; printf 'steps:\\n  - id: build\\n    agent: pi\\n    prompt: late-prompt\\n    model: m\\n    on_pass: finish\\n  - id: finish\\n    run: x\\n    on_pass: done\\n' >'$LATE'; printf '\\r'; sleep 2; } | '$SPOOLWAY'" "$RUN"
sed 's/\x1b\[[0-9;]*m//g' "$RUN" >"$RUN.plain"
rm -f "$LATE"
has "the popup names the prompt of the pipeline added since" "late-prompt" "$RUN.plain"

# One `spoolway` per project. A screen held open by a pipe that stays open
# for a few seconds takes `spoolway.pid`; while it is up, a second bare
# `spoolway` and a typed `spoolway dispatch` both refuse with the one line.
# A real second process against a real one holding the lock, which a unit
# test's in-process lock cannot stand in for.
SCREEN_LOCK="$SPOOLWAY_PROJECT_HOME/spoolway.pid"
screen_held() {
  local p
  p=$(head -1 "$SCREEN_LOCK" 2>/dev/null) && [ -n "$p" ] && kill -0 "$p" 2>/dev/null
}
HELD="$LIVE/held.txt"
script -qec "sleep 8 | '$SPOOLWAY'" "$HELD" >/dev/null 2>&1 &
HELD_PID=$!
if poll_until 10 screen_held; then ok "an open screen holds spoolway.pid"
else bad "an open screen holds spoolway.pid"; fi

SECOND="$LIVE/second.txt"
works "a second bare spoolway in the same project ends on its own" \
  script -qec "printf '' | '$SPOOLWAY'" "$SECOND"
sed 's/\x1b\[[0-9;]*m//g' "$SECOND" >"$SECOND.plain"
has "and says the one line" "Dispatcher already running" "$SECOND.plain"
lacks "without drawing a screen" "DISPATCH       [QUEUE]       ROUTINES        JOBS        EVAL" "$SECOND.plain"
says "spoolway dispatch refuses while the screen is open" \
  "Dispatcher already running" "$SPOOLWAY" dispatch
exit_code "with the lock's own exit code" 4 "$SPOOLWAY" dispatch

wait "$HELD_PID"
if poll_until 10 bash -c '! kill -0 "$(head -1 "$1" 2>/dev/null)" 2>/dev/null' _ "$SCREEN_LOCK"
then ok "the screen lets go of spoolway.pid when it quits"
else bad "the screen lets go of spoolway.pid when it quits"; fi
AFTER="$LIVE/after.txt"
works "a screen opened after it opens as usual" \
  script -qec "printf '' | '$SPOOLWAY'" "$AFTER"
sed 's/\x1b\[[0-9;]*m//g' "$AFTER" >"$AFTER.plain"
lacks "not refused" "Dispatcher already running" "$AFTER.plain"
has "drawing the strip" "DISPATCH       [QUEUE]       ROUTINES        JOBS        EVAL" "$AFTER.plain"

# A refusal on the queue tab is a popup over the tab, not a frame of its own:
# a group whose document sets a key spoolway reserves is refused at `enter`.
# Written last, so it is the newest group and the one the cursor opens on;
# `space` selects it and `enter` submits it. The pipe then runs out with the
# popup still up — it closes on `enter` alone — so the last frame is the one
# with the popup on it.
pending_doc bad-stage "$BODY" "group: refused" "stage: taken"
REFUSED="$LIVE/refused.txt"
works "a refused submission on the queue tab ends when its keys run out" \
  script -qec "printf ' \\r' | '$SPOOLWAY'" "$REFUSED"
awk 'BEGIN { RS = "\033\\[\\?2026h\033\\[H" } { last = $0 } END { print last }' "$REFUSED" |
  sed 's/\x1b\[[0-9;]*m//g' >"$LAST"
has "the refusal is drawn in a popup" "╭─ submission refused " "$LAST"
has "naming the reserved key" "stage" "$LAST"
has "answered by enter" "[enter] confirm" "$LAST"
has "over the queue tab, still drawn under it" "─ groups" "$LAST"
has "under the strip" "DISPATCH       [QUEUE]       ROUTINES        JOBS        EVAL" "$LAST"
works "nothing was queued" test ! -e "$SPOOLWAY_PROJECT_HOME/queue/bad-stage.md"

# A clean submission lands on the queued popup, which ends by saying whether a
# dispatcher will pick the work up. The dispatcher started above has stopped,
# so nothing holds its lock and the popup asks for one to be started. The
# refused group is cleared first so the new one is the only group written
# since `cart`, and the one the cursor opens on.
rm -f "$SPOOLWAY_PROJECT_HOME/pending/bad-stage.md"
pending_doc billing-export "$BODY" "group: billing"
QUEUED="$LIVE/queued.txt"
works "a clean submission on the queue tab ends when its keys run out" \
  script -qec "printf ' \\r' | '$SPOOLWAY'" "$QUEUED"
awk 'BEGIN { RS = "\033\\[\\?2026h\033\\[H" } { last = $0 } END { print last }' "$QUEUED" |
  sed 's/\x1b\[[0-9;]*m//g' >"$LAST"
has "the queued popup is drawn over the tab" "╭─ queued " "$LAST"
has "naming the task it queued" "billing-export" "$LAST"
has "saying no dispatcher will pick it up yet" "Start the dispatcher to begin working" "$LAST"
lacks "never that one is running" "Dispatcher is running" "$LAST"
has "answered by enter" "[enter] confirm" "$LAST"
works "the task was queued" test -e "$SPOOLWAY_PROJECT_HOME/queue/billing-export.md"

# A failed task hook is named once, when a dispatcher is asked for, and never
# on the board. `billing-export` is queued above, so a non-zero `.exit` for
# its `started` hook under `tracking/` is a failure the popup has to name
# under `problems`. `fetch-*` runs belong to `spoolway issue show` and give
# no row. The pipe sends `←` to reach the dispatch tab and `enter` to ask,
# then runs out with the popup still up, so the last frame holds both the
# popup and the board footer under it. Wording is checked, not line breaks:
# the row wraps at the pane's width.
TRACKING="$SPOOLWAY_PROJECT_HOME/tracking"
mkdir -p "$TRACKING"
printf '1\n' >"$TRACKING/billing-export · started.exit"
printf 'hook said no\n' >"$TRACKING/billing-export · started.log"
printf '1\n' >"$TRACKING/fetch-acme-app-7.exit"
HOOKFAIL="$LIVE/hookfail.txt"
works "asking to dispatch with a failed hook on a queued task ends when its keys run out" \
  script -qec "{ printf '\\033[D'; sleep 1; printf '\\r'; sleep 2; } | '$SPOOLWAY'" "$HOOKFAIL"
awk 'BEGIN { RS = "\033\\[\\?2026h\033\\[H" } { last = $0 } END { print last }' "$HOOKFAIL" |
  sed 's/\x1b\[[0-9;]*m//g' >"$LAST"
# Words wrap between lines, with the popup's border and the cursor save,
# erase and restore codes and carriage returns between them, so the frame is joined into one line
# with those taken out first.
HOOKFLAT="$LIVE/hookfail.flat"
sed -e 's/\x1b[78]//g' -e 's/\x1b\[K//g' -e 's/│//g' "$LAST" | tr '\r\n' '  ' | tr -s ' ' >"$HOOKFLAT"
has "the before-dispatching popup is drawn" "─ before dispatching " "$LAST"
has "it has a problems section" "problems" "$LAST"
has "it names the hook that failed" "issue_tracking hooks" "$HOOKFLAT"
has "and the event" '`started` failed for billing-export' "$HOOKFLAT"
has "and the exit code" "(exit 1)" "$HOOKFLAT"
has "and what clears it" '`spoolway resume billing-export`' "$HOOKFLAT"
has "and where its log is" "started.log" "$HOOKFLAT"
lacks "the board draws no hook-failure line" "hook failures" "$LAST"
lacks "and no pointer at tracking/" "see tracking/" "$LAST"
lacks "a fetch run gives no row" "fetch-acme-app-7" "$LAST"
rm -f "$TRACKING/billing-export · started.exit" "$TRACKING/billing-export · started.log" \
  "$TRACKING/fetch-acme-app-7.exit"

# Every tab, both directions, on a real pty — the acceptance criterion the
# task `frames-onto-writer` exists for: no redrawing screen erases the whole
# terminal any more, all five of them painting through the shared frame
# writer instead (`src/screen/frame_writer.rs`). `←` from the queue tab
# reaches dispatch, `→` four times walks back through queue, routines, jobs
# and eval, and `←` four times walks all the way back — every tab this screen
# has, both ways, with no `enter` anywhere so nothing is started or queued.
ALL_TABS="$LIVE/all-tabs.txt"
works "cycling every tab does not crash the screen" \
  script -qec "printf '\\033[D\\033[C\\033[C\\033[C\\033[C\\033[D\\033[D\\033[D\\033[D' | '$SPOOLWAY'" "$ALL_TABS"
if grep -aqF $'\x1b[2J' "$ALL_TABS"; then
  bad "no frame drawn while cycling every tab still erases the whole screen"
  tail -30 "$ALL_TABS" | cat -v
else
  ok "no frame drawn while cycling every tab still erases the whole screen"
fi
sed 's/\x1b\[[0-9;]*m//g' "$ALL_TABS" >"$ALL_TABS.plain"
# The check above passes on an empty capture too — a screen that crashed or
# quit before drawing a single tab has no `ESC[2J` in it either. These five
# require every tab this walk visits actually drew, so the check above is
# proof about frames that were really there.
has "the walk actually reached the dispatch tab" "dispatcher stopped" "$ALL_TABS.plain"
has "and the queue tab" "─ groups" "$ALL_TABS.plain"
has "and the routines tab" "QUEUE       [ROUTINES]       JOBS" "$ALL_TABS.plain"
has "and the jobs tab" "no jobs yet" "$ALL_TABS.plain"
has "and the eval tab" "╭─ eval ·" "$ALL_TABS.plain"

# `→` from the queue tab, where the screen opens, reaches the routines tab
# between queue and jobs, listing the one routine under `.spoolway/routines/`.
# Written last, so no check above runs with a routine in the checkout.
mkdir -p .spoolway/routines/nightly
task_doc .spoolway/routines/nightly/audit-deps.md audit-deps "$BODY" "group: nightly"
ROUTINES="$LIVE/routines.txt"
works "→ from the queue tab ends when its keys run out" \
  script -qec "printf '\\033[C' | '$SPOOLWAY'" "$ROUTINES"
awk 'BEGIN { RS = "\033\\[\\?2026h\033\\[H" } { last = $0 } END { print last }' "$ROUTINES" |
  sed 's/\x1b\[[0-9;]*m//g' >"$LAST"
has "→ reaches the routines tab" \
  "DISPATCH        QUEUE       [ROUTINES]       JOBS        EVAL" "$LAST"
has "titled routines" "─ routines ─" "$LAST"
has "listing the routine" "> [ ] nightly" "$LAST"
has "under its own key line" \
  "[space] select   [enter] queue   [n] new job   [x] delete   [tab] tasks   [q] quit" "$LAST"

# A full board in a short pane scrolls its task table rather than cutting the
# frame off at the bottom. Thirty queued tasks in six groups are more than a
# 30-row terminal leaves the dispatch tab's table, so its last table row is a
# marker counting the tasks under it, and the key line is still drawn under
# the box. `stty` sizes the pty `script` opens before the screen measures it;
# nothing is dispatched, so the tasks stay where they were written.
for g in 1 2 3 4 5 6; do
  for t in 1 2 3 4 5; do
    task_doc "$SPOOLWAY_PROJECT_HOME/queue/full-$g-$t.md" "full-$g-$t" "$BODY" \
      "stage: queued" "group: full-$g"
  done
done
FULL="$LIVE/full.txt"
works "a full board in a short pane ends when its keys run out" \
  script -qec "stty rows 30 cols 120; printf '\\033[D' | '$SPOOLWAY'" "$FULL"
awk 'BEGIN { RS = "\033\\[\\?2026h\033\\[H" } { last = $0 } END { print last }' "$FULL" |
  sed 's/\x1b\[[0-9;]*m//g' >"$LAST"
has "the full board is the dispatch tab's" "← [DISPATCH]       QUEUE" "$LAST"
has "its cursor's row is drawn" "▸ " "$LAST"
has "its last table row counts the tasks out of view" "tasks below" "$LAST"
has "the key line is still drawn under it" "[o] open task" "$LAST"

# The margin, read off a real pty rather than a test terminal: at 120 columns
# no row of the full board touches column 0 or column 119. Each row ends in the
# writer's save-cursor, clear-to-end, restore-cursor, which is taken out before
# a row is measured, and its width is counted in characters, not bytes, since
# the boxes are drawn in multi-byte glyphs. The rows stop at the key line's last
# row, before `script`'s own closing lines. The two blank rows under the key
# line are cleared rather than written, so a transcript cannot show them; the
# unit test over every tab decides those.
tr -d '\r' <"$LAST" | sed -e 's/\x1b7\x1b\[K\x1b8//g' -e 's/\x1b\[[0-9;?]*[A-Za-z]//g' | sed '/\[q\] quit/q' >"$LAST.rows"
EDGE=$(LC_ALL=C.UTF-8 awk '/[^[:space:]]/ && (substr($0, 1, 1) != " " || length($0) > 119) { n++ } END { print n + 0 }' "$LAST.rows")
if [ "$EDGE" = 0 ]; then ok "no row of the full board touches either edge of a 120-column terminal"
else bad "no row of the full board touches either edge of a 120-column terminal ($EDGE rows do)"; head -40 "$LAST.rows"; fi
rm -f "$SPOOLWAY_PROJECT_HOME"/queue/full-*.md

# Off a terminal: the grouped help, on stderr, the way it always was.
HELP="$LIVE/help.txt"
"$SPOOLWAY" 2>"$HELP" | cat >/dev/null
has "spoolway | cat still prints the grouped help" "Your work:" "$HELP"

finish
