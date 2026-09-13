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

# A one-shot board dispatcher — the same shape `disaster.sh`'s own
# `one_shot_start_board`/`one_shot_stop` use, repeated here rather than
# shared: this suite needs it once, to read the dispatcher board's job
# ledger, which `dispatcher_start`'s own `--plain` mode never draws.
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
one_shot_stop() {
  local pid=$1
  [ -n "$pid" ] || return 0
  kill -INT "$pid" 2>/dev/null
  poll_while 15 kill -0 "$pid"
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

# A dispatcher pass against a matching minute fires the job.
dispatcher_start

poll_until 20 test -f "$SPOOLWAY_PROJECT_HOME/queue/audit-docs-1.md" \
  && ok "the job fired: the routine's second document reached the queue under a minted id" \
  || bad "the job never queued audit-docs-1 (dispatch log follows)"

works "and the first document too, in the queue or already archived" \
  bash -c '[ -f "$1/queue/audit-deps-1.md" ] || [ -f "$1/archive/audit-deps-1.md" ]' \
  _ "$SPOOLWAY_PROJECT_HOME"

works "never the routine documents' own bare ids" \
  bash -c '[ ! -e "$1/queue/audit-deps.md" ] && [ ! -e "$1/queue/audit-docs.md" ]' \
  _ "$SPOOLWAY_PROJECT_HOME"

has "the minted copy keeps the document's own group" "group: nightly" \
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
# above the key controls — whether the queue is empty or has work in it. No
# `covers:` tag here either, for the same reason the top of this file gives.
#
# `nightly-audit` moves off `* * * * *` first, straight in the store the same
# way it was written: this section's own one-shot board is a real dispatcher,
# and a job still due every minute would refire into the very queue this
# section empties out from under it, racing the "empty board" case below
# against its own schedule.
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

BEFORE=$(wc -l < "$E2E_DISPATCH_LOG" 2>/dev/null || echo 0)
BOARD_PID=$(one_shot_start_board)
if wait_for_text 15 "$E2E_DISPATCH_LOG" "2 active"; then
  ok "a busy board draws the job ledger"
else
  bad "a busy board draws the job ledger"
fi
tail -n +"$((BEFORE + 1))" "$E2E_DISPATCH_LOG" > "$LIVE/busy-board.out"
if grep -qF "2 active" "$LIVE/busy-board.out" \
   && grep -qF "nightly-audit" "$LIVE/busy-board.out" \
   && grep -qF "release-readiness" "$LIVE/busy-board.out"; then
  ok "the busy board's ledger names both enabled jobs"
else
  bad "the busy board's ledger names both enabled jobs"
  sed 's/^/        /' "$LIVE/busy-board.out"
fi
one_shot_stop "$BOARD_PID"

# Empty the queue outright — everything this suite has queued so far, fired
# copies included — so the very same board is read again with nothing left
# to work on.
rm -f "$SPOOLWAY_PROJECT_HOME"/queue/*.md

BEFORE=$(wc -l < "$E2E_DISPATCH_LOG" 2>/dev/null || echo 0)
BOARD_PID=$(one_shot_start_board)
if wait_for_text 15 "$E2E_DISPATCH_LOG" "queue is empty"; then
  ok "an empty board still says why it is resident"
else
  bad "an empty board still says why it is resident"
fi
tail -n +"$((BEFORE + 1))" "$E2E_DISPATCH_LOG" > "$LIVE/empty-board.out"
if grep -qF "2 active" "$LIVE/empty-board.out" \
   && grep -qF "nightly-audit" "$LIVE/empty-board.out" \
   && grep -qF "release-readiness" "$LIVE/empty-board.out"; then
  ok "the empty board's ledger names both enabled jobs too"
else
  bad "the empty board's ledger names both enabled jobs too"
  sed 's/^/        /' "$LIVE/empty-board.out"
fi
# `spoolway dispatch` prints one plain "queue is empty" line at process
# startup, before the board ever draws a frame — the same announcement
# `--plain` prints, "next: ..." line included, and a real terminal never
# shows it past the first redraw's screen clear. Only the drawn frames
# themselves are this task's own claim, so the check reads past the first
# `\x1b[2J\x1b[H` rather than the whole capture.
if python3 - "$LIVE/empty-board.out" <<'PY'
import sys
data = open(sys.argv[1], "rb").read()
frames = data.split(b"\x1b[2J\x1b[H")[1:]
body = b"\x1b[2J\x1b[H".join(frames)
sys.exit(1 if b"next: nightly-audit," in body else 0)
PY
then
  ok "the empty-queue copy does not repeat a job's next firing the ledger already names"
else
  bad "the empty-queue copy does not repeat a job's next firing the ledger already names"
  sed 's/^/        /' "$LIVE/empty-board.out"
fi
one_shot_stop "$BOARD_PID"

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
