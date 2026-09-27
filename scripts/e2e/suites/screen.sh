#!/usr/bin/env bash
# Bare `spoolway`: the one screen with its dispatch, queue, jobs and eval
# tabs, driven end to end as the whole binary. It opens only with stdout on a
# terminal, so the run is wrapped in `script`, which gives it a pty for stdout
# while the keys still arrive on an ordinary pipe — the same way every other
# screen suite scripts its keystrokes.
#
# What is asserted is the order of the frames it drew: the first is the queue
# tab, and `←` from there draws the dispatch tab's board, where `enter` starts
# a dispatcher and, behind a popup asking how, stops it. While one screen is open, a second `spoolway` or
# `spoolway dispatch` in the same project refuses. A refusal on the queue tab
# is drawn in a popup over the tab. Off a terminal, bare `spoolway` still
# prints the grouped help.
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

# Every frame opens on a clear-screen, so the transcript splits into frames on
# it. The first one drawn is the queue tab's. Colour codes are taken out, so
# the strip's labels read as the plain line the mockup draws.
FIRST="$LIVE/first.txt"
LAST="$LIVE/last.txt"
awk 'BEGIN { RS = "\033\\[2J\033\\[H" } NR == 2 { print; exit }' "$DRAWN" |
  sed 's/\x1b\[[0-9;]*m//g' >"$FIRST"
awk 'BEGIN { RS = "\033\\[2J\033\\[H" } { last = $0 } END { print last }' "$DRAWN" |
  sed 's/\x1b\[[0-9;]*m//g' >"$LAST"

has "the strip names all four tabs in order" \
  "dispatch        queue        jobs        eval" "$FIRST"
has "it opens on the queue tab" "groups  1 of 1" "$FIRST"
has "with the pending group listed" "cart" "$FIRST"
lacks "not on the dispatch tab" "dispatcher" "$FIRST"

has "← reaches the dispatch tab's board" "dispatcher stopped" "$LAST"
lacks "which is no longer the queue tab" "groups  1 of 1" "$LAST"

# `enter` on the dispatch tab starts a `spoolway dispatch` child and `enter`
# again asks how to stop it — even with nothing running — and `enter` on that
# popup stops it, letting running steps finish. This project's fake models
# give the warnings gate something to say, so the first `enter` opens that
# popup first and `x` answers it — hiding it and starting the child. The keys
# are paced so the child has time to take the lock before the second `enter`,
# and time to go once asked; the pipe then runs out and the screen ends.
# Nothing is queued, which a child the screen started waits on rather than
# exiting — so a frame naming its pid while the queue is empty is also the
# proof it stayed up.
RUN="$LIVE/run.txt"
works "enter on the dispatch tab starts dispatching, and enter then enter stops it" \
  script -qec "{ printf '\\033[D\\r'; sleep 1; printf x; sleep 5; printf '\\r'; sleep 1; printf '\\r'; sleep 3; } | '$SPOOLWAY'" "$RUN"
sed 's/\x1b\[[0-9;]*m//g' "$RUN" >"$RUN.plain"
awk 'BEGIN { RS = "\033\\[2J\033\\[H" } { last = $0 } END { print last }' "$RUN" |
  sed 's/\x1b\[[0-9;]*m//g' >"$LAST"

has "enter asks the warnings gate first, as a popup" "─ before dispatching " "$RUN.plain"
has "whose keys read as drawn" "[enter] start dispatching   [esc] back   [x] hide until these change" "$RUN.plain"
has "the header names the child's pid while it runs" "dispatcher running · pid " "$RUN.plain"
has "and enter is offered to stop it" "[enter] stop dispatching" "$RUN.plain"
has "and it stays up on an empty queue" "nothing queued" "$RUN.plain"
has "enter over it asks how to stop, as a popup" "┌─ stop dispatching " "$RUN.plain"
has "saying no new steps will start" "No new steps will be started." "$RUN.plain"
has "offering enter to let running steps finish" "[enter] let running steps finish" "$RUN.plain"
has "and i to interrupt them, or esc" "[i] interrupt them now   [esc] back" "$RUN.plain"
lacks "the start was not refused" "the dispatcher did not start" "$RUN.plain"
has "enter on the popup stops it" "dispatcher stopped" "$LAST"
lacks "and closes the popup" "No new steps will be started." "$LAST"
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
lacks "without drawing a screen" "dispatch        queue        jobs        eval" "$SECOND.plain"
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
has "drawing the strip" "dispatch        queue        jobs        eval" "$AFTER.plain"

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
awk 'BEGIN { RS = "\033\\[2J\033\\[H" } { last = $0 } END { print last }' "$REFUSED" |
  sed 's/\x1b\[[0-9;]*m//g' >"$LAST"
has "the refusal is drawn in a popup" "┌─ submission refused " "$LAST"
has "naming the reserved key" "stage" "$LAST"
has "closed by enter" "[enter] close" "$LAST"
has "over the queue tab, still drawn under it" "─ groups" "$LAST"
has "under the strip" "dispatch        queue        jobs        eval" "$LAST"
works "nothing was queued" test ! -e "$SPOOLWAY_PROJECT_HOME/queue/bad-stage.md"

# Off a terminal: the grouped help, on stderr, the way it always was.
HELP="$LIVE/help.txt"
"$SPOOLWAY" 2>"$HELP" | cat >/dev/null
has "spoolway | cat still prints the grouped help" "Your work:" "$HELP"

finish
