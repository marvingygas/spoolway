#!/usr/bin/env bash
# Cron jobs, fired by the dispatcher's own pass.
#
# `routines.sh` proves a person queueing a routine folder by hand lands the
# right task files under minted ids. This proves the engine does the same on
# a schedule: a job in a store, a dispatcher pass against a minute the job's
# expression matches, and the routine's documents in the queue under freshly
# minted ids — never the bare ids the routine's own documents carry.
#
# `* * * * *` matches every minute, so the first pass fires. The chain
# (audit-docs depends on audit-deps) keeps audit-docs sitting in the queue
# while audit-deps runs, so it is there to assert on whichever order the
# dispatcher gets to them.
#
# No `covers:` tag — a job is neither a config.toml key nor a pipeline step
# key, the same reasoning `routines.sh` gives.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"
# shellcheck source=../fixture.sh
source "$HERE/../fixture.sh"
# shellcheck source=../agents.sh
source "$HERE/../agents.sh"

LIVE=${WORK:-$(mktemp -d)}

install_agents "$LIVE/bin" "$LIVE/ctl"
new_repo "$LIVE/proj"
configure_project plan/live

BODY="$LIVE/body.md"
task_body "$BODY"

# The board's job ledger is read off bare `spoolway`'s dispatch tab, kept open
# by `lib.sh`'s `screen_start` and walked onto the tab by `screen_board`. A
# busy board draws the ledger whether or not anything is dispatching, so the
# tab starts no run for it; an empty board draws it only while a dispatcher
# holds the lock, so the tab starts one there with `screen_dispatch`.
BOARD_LOG="$LIVE/board.out"
board_open() {
  screen_start "$BOARD_LOG"
  screen_board
}

# The routine a job points at: two documents straight in `nightly/`, the
# second depending on the first, so firing the job has to remap that
# `depends_on` onto the ids it mints.
mkdir -p .spoolway/routines/nightly
task_doc .spoolway/routines/nightly/audit-deps.md audit-deps "$BODY" "group: nightly"
task_doc .spoolway/routines/nightly/audit-docs.md audit-docs "$BODY" \
  "group: nightly" "depends_on: [audit-deps]"

# The job, written straight into the user-scoped store — the screen that
# writes one is a later task, so the suite writes the TOML itself. A second,
# always-enabled job alongside it — never due during this suite's run — so
# the dispatcher board's job ledger further down has two rows to show, not
# one.
cat > "$SPOOLWAY_PROJECT_HOME/jobs.toml" <<TOML
[jobs.nightly-audit]
schedule = "* * * * *"
pipeline = "default"
routine = "nightly"

[jobs.release-readiness]
schedule = "0 8 * * 1"
pipeline = "default"
routine = "nightly"
TOML

works "jobs list shows the job" \
  bash -c '"$1" jobs list | grep -q nightly-audit' _ "$SPOOLWAY"
says "jobs list --json carries the schedule" '"schedule": "* * * * *"' \
  "$SPOOLWAY" jobs list --json

# `jobs contract` names this project's own two store paths — the project one
# relative to the checkout, the user one `~`-shortened under home, the same
# machine home `new_repo` set `SPOOLWAY_PROJECT_HOME` to.
USER_STORE_DISPLAY="~/.spoolway/$(basename "$SPOOLWAY_PROJECT_HOME")/jobs.toml"
says "jobs contract names the project store" '.spoolway/jobs.toml' \
  "$SPOOLWAY" jobs contract
says "and the user store" "$USER_STORE_DISPLAY" \
  "$SPOOLWAY" jobs contract

# A dispatcher pass against a matching minute fires the job.
dispatcher_start

poll_until 20 test -f "$SPOOLWAY_PROJECT_HOME/queue/audit-docs-1.md" \
  && ok "the job fired: the routine's second document reached the queue under a minted id" \
  || bad "the job never queued audit-docs-1 (dispatch log follows)"

works "and the first task too, in the queue or already archived" \
  bash -c '[ -f "$1/queue/audit-deps-1.md" ] || [ -f "$1/archive/audit-deps-1.md" ]' \
  _ "$SPOOLWAY_PROJECT_HOME"

works "never the routine tasks' own bare ids" \
  bash -c '[ ! -e "$1/queue/audit-deps.md" ] && [ ! -e "$1/queue/audit-docs.md" ]' \
  _ "$SPOOLWAY_PROJECT_HOME"

has "the minted copy keeps the task's own group" "group: nightly" \
  "$SPOOLWAY_PROJECT_HOME/queue/audit-docs-1.md"
has "and is put on the job's pipeline" "pipeline: default" \
  "$SPOOLWAY_PROJECT_HOME/queue/audit-docs-1.md"
has "its depends_on is remapped onto the sibling's minted id" "audit-deps-1" \
  "$SPOOLWAY_PROJECT_HOME/queue/audit-docs-1.md"

works "the routine tree is left exactly where it was" \
  test -f .spoolway/routines/nightly/audit-deps.md
has "unminted, unmodified" "id: audit-deps" \
  .spoolway/routines/nightly/audit-deps.md

dispatcher_stop

# ------------- the dispatcher board's job ledger, busy and empty
# Every enabled job is on the board's own ledger — beneath the slot lines,
# above the key controls — while the queue has work in it. Once it is empty,
# the ledger stays under the greeting only while a dispatcher holds the lock,
# since no job fires without one. No `covers:` tag here either, for the same
# reason the top of this file gives.
#
# The busy board is read with nothing dispatching, so the ledger is all that
# is being watched. `nightly-audit` still moves off `* * * * *` first,
# straight in the store the same way it was written, so no job is due while
# the board is read at all — not even once the dispatcher below is up.
cat > "$SPOOLWAY_PROJECT_HOME/jobs.toml" <<TOML
[jobs.nightly-audit]
schedule = "0 3 * * *"
pipeline = "default"
routine = "nightly"

[jobs.release-readiness]
schedule = "0 8 * * 1"
pipeline = "default"
routine = "nightly"
TOML
task_doc "$LIVE/ledger-busy.md" ledger-busy "$BODY" "group: ledger-busy"
must "a plain task queues, to keep the board busy" \
  "$SPOOLWAY" queue add --from "$LIVE/ledger-busy.md"

BEFORE=$(wc -l < "$BOARD_LOG" 2>/dev/null || echo 0)
board_open
if poll_until 15 screen_drew_since "$BEFORE" "2 active"; then
  ok "a busy board draws the job ledger"
else
  bad "a busy board draws the job ledger"
fi
tail -n +"$((BEFORE + 1))" "$BOARD_LOG" > "$LIVE/busy-board.out"
if grep -qaF "2 active" "$LIVE/busy-board.out" \
   && grep -qaF "nightly-audit" "$LIVE/busy-board.out" \
   && grep -qaF "release-readiness" "$LIVE/busy-board.out"; then
  ok "the busy board's ledger names both enabled jobs"
else
  bad "the busy board's ledger names both enabled jobs"
  sed 's/^/        /' "$LIVE/busy-board.out"
fi
screen_stop

# Empty the queue outright — everything this suite has queued so far, fired
# copies included — so the very same board is read again with nothing left
# to work on.
rm -f "$SPOOLWAY_PROJECT_HOME"/queue/*.md

BEFORE=$(wc -l < "$BOARD_LOG" 2>/dev/null || echo 0)
board_open
if poll_until 15 screen_drew_since "$BEFORE" "Nothing queued"; then
  ok "an empty board still says Nothing queued with jobs enabled"
else
  bad "an empty board still says Nothing queued with jobs enabled"
fi
tail -n +"$((BEFORE + 1))" "$BOARD_LOG" > "$LIVE/stopped-board.out"
# With no dispatcher up nothing will fire, so the stopped board lists no job;
# the jobs tab still lists every job and its next firing.
lacks "the stopped empty board leaves the job ledger off" "2 active" \
  "$LIVE/stopped-board.out"

screen_stop

# The same empty board, once a dispatcher holds the lock: the ledger comes
# back under the greeting, in the busy board's words. The tab starts the
# dispatcher itself — a `spoolway dispatch` of the suite's own is refused while
# the screen is open — and it stays up on the empty queue.
BEFORE=$(wc -l < "$BOARD_LOG" 2>/dev/null || echo 0)
screen_start "$BOARD_LOG"
screen_dispatch
if poll_until 15 screen_drew_since "$BEFORE" "2 active"; then
  ok "an idle dispatcher's empty board draws the job ledger"
else
  bad "an idle dispatcher's empty board draws the job ledger"
fi
tail -n +"$((BEFORE + 1))" "$BOARD_LOG" > "$LIVE/empty-board.out"
if grep -qaF "nightly-audit" "$LIVE/empty-board.out" \
   && grep -qaF "release-readiness" "$LIVE/empty-board.out"; then
  ok "the idle board's ledger names both enabled jobs"
else
  bad "the idle board's ledger names both enabled jobs"
  sed 's/^/        /' "$LIVE/empty-board.out"
fi
# The plain run prints a "next: ..." line under its own "nothing queued"
# line; the board draws the ledger instead, never that line.
lacks "the empty board does not draw the plain run's next line" \
  "next: nightly-audit," "$LIVE/empty-board.out"
screen_stop

# `spoolway doctor` names a job whose expression will not parse, one whose
# expression parses but never comes round, one whose routine is gone, and one
# whose pipeline is not defined.
cat >> "$SPOOLWAY_PROJECT_HOME/jobs.toml" <<TOML

[jobs.broken]
schedule = "99 * * * *"
pipeline = "no-such-pipeline"
routine = "ghost"
enabled = false

[jobs.impossible]
schedule = "0 0 30 2 *"
pipeline = "default"
routine = "nightly"
enabled = false
TOML

refuses "doctor fails when a job is broken" "problem" "$SPOOLWAY" doctor
says "— naming the unparseable expression" "job \`broken\` schedule fires" \
  "$SPOOLWAY" doctor
says "— naming a schedule that never comes round" "\`0 0 30 2 *\` parses but never comes round" \
  "$SPOOLWAY" doctor
says "— naming the routine that is gone" "job \`broken\` routine exists" \
  "$SPOOLWAY" doctor
says "— naming the pipeline that is not defined" "job \`broken\` pipeline is defined" \
  "$SPOOLWAY" doctor

finish
