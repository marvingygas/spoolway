#!/usr/bin/env bash
# Bare `spoolway`: the one screen with its dispatch, queue, jobs and eval
# tabs, driven end to end as the whole binary. It opens only with stdout on a
# terminal, so the run is wrapped in `script`, which gives it a pty for stdout
# while the keys still arrive on an ordinary pipe — the same way every other
# screen suite scripts its keystrokes.
#
# What is asserted is the order of the frames it drew: the first is the queue
# tab, and `←` from there draws the dispatch tab's board. Off a terminal, bare
# `spoolway` still prints the grouped help.
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

new_repo "$LIVE/proj"
configure_project plan/live

# One pending group, so the queue tab has a row of its own to draw.
BODY="$LIVE/body.md"
task_body "$BODY"
pending_doc cart-totals "$BODY" "group: cart" "touches: [notes/cart.md]"

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

# Off a terminal: the grouped help, on stderr, the way it always was.
HELP="$LIVE/help.txt"
"$SPOOLWAY" 2>"$HELP" | cat >/dev/null
has "spoolway | cat still prints the grouped help" "Your work:" "$HELP"

finish
