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

# A routine of its own for the second job: a routine has at most one job.
mkdir -p .spoolway/routines/weekly
task_doc .spoolway/routines/weekly/release-check.md release-check "$BODY" "group: weekly"

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
base = "plan/live"

[jobs.release-readiness]
schedule = "0 8 * * 1"
pipeline = "default"
routine = "weekly"
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

# The main checkout is put on a branch of its own, one local commit ahead, for
# the firing: a job's tasks are cut from its configured `base`, never from
# whatever branch the checkout happens to have out.
ORIGINAL_BRANCH=$(git rev-parse --abbrev-ref HEAD)
must "the main checkout moves to a branch of its own" \
  git checkout -q -b experiment/local-only
must "with a local commit on it" \
  git commit -q --allow-empty -m "local work in progress"

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
has "it is cut from the job's base, not the branch the checkout has out" \
  "base: plan/live" "$SPOOLWAY_PROJECT_HOME/queue/audit-docs-1.md"
lacks "and never from that branch" "experiment/local-only" \
  "$SPOOLWAY_PROJECT_HOME/queue/audit-docs-1.md"

works "the routine tree is left exactly where it was" \
  test -f .spoolway/routines/nightly/audit-deps.md
has "unminted, unmodified" "id: audit-deps" \
  .spoolway/routines/nightly/audit-deps.md

dispatcher_stop
must "the main checkout goes back to the branch it started on" \
  git checkout -q "$ORIGINAL_BRANCH"

# ------------- a job firing under a hook with an unmet tool requirement
# A job fires with nobody watching, so the tool-requirements gate must not
# wait on a key for it, even when the dispatcher's own stdin is a terminal —
# a dispatcher running in a terminal pane. The firing prints the gate's notice
# once, queues its task with `tracking: off`, and records the minute as fired.
#
# The dispatcher runs under a real pty here (`script`), its stdin the pty
# rather than the pipe or `/dev/null` every other dispatcher in this suite
# has. Nothing ever types into it: a gate that waits on a key waits forever,
# and the task never reaches the queue.
mkdir -p .spoolway/hooks .spoolway/routines/gated
cat > .spoolway/hooks/needs-tool.sh <<'EOF'
#!/usr/bin/env bash
# spoolway-requires: nosuchtool >= 1.0
case "$SPOOLWAY_EVENT" in
  fetch) exit 0 ;;
esac
exit 0
EOF
chmod +x .spoolway/hooks/needs-tool.sh
must "the hook that requires a missing tool is named" \
  "$SPOOLWAY" config set issue_tracking.hook needs-tool.sh
must "with a project key, so doctor has no complaint about the pair" \
  "$SPOOLWAY" config set issue_tracking.project_key E2E
must "and no ticket slug in names, which the hook would have to write" \
  "$SPOOLWAY" config set issue_tracking.key_in_names false
task_doc .spoolway/routines/gated/gated-audit.md gated-audit "$BODY" "group: gated"

cat > "$SPOOLWAY_PROJECT_HOME/jobs.toml" <<TOML
[jobs.gated-job]
schedule = "* * * * *"
pipeline = "default"
routine = "gated"
base = "plan/live"
TOML

GATED_LOG="$LIVE/gated-dispatch.out"
GATED_PID="$LIVE/gated-dispatch.pid"
GATED_FIFO="$LIVE/gated-dispatch.keys"
: > "$GATED_LOG"
rm -f "$GATED_PID" "$GATED_FIFO"
mkfifo "$GATED_FIFO"
# Held open read-write on fd 8, so `script` never reads EOF off its stdin.
exec 8<>"$GATED_FIFO"
env SPOOLWAY_SKIP_VERSION_CHECK=1 E2E_BIN="$SPOOLWAY" E2E_PIDFILE="$GATED_PID" \
  setsid script -qfec 'stty cols 160 rows 50; echo $$ >"$E2E_PIDFILE"; exec "$E2E_BIN" dispatch' \
  "$GATED_LOG" <"$GATED_FIFO" >/dev/null 2>&1 &
disown
poll_until 10 test -s "$GATED_PID" || bad "the pty dispatcher never started"
# On a terminal `dispatch` first asks doctor's start gate. `↓` `enter` picks
# its second answer, hiding what it shows, as `screen_dispatch` does. The gate
# is answered before any pass runs, so a firing that then waited on a key
# would still sit on its own prompt, which `queue anyway` below looks for.
poll_until 10 grep -qaF "hide until these change" "$GATED_LOG" && printf '\033[B\r' >&8

gated_queued() {
  compgen -G "$SPOOLWAY_PROJECT_HOME/queue/gated-audit-*.md" >/dev/null \
    || compgen -G "$SPOOLWAY_PROJECT_HOME/archive/gated-audit-*.md" >/dev/null
}
if poll_until 20 gated_queued; then
  ok "a job firing under a hook with an unmet tool queued its task without waiting on a key"
else
  bad "the job never queued its task: the pass is waiting on a key (dispatcher output follows)"
  sed 's/^/        /' "$GATED_LOG" | tail -30
fi

GATED_TASK=$(ls "$SPOOLWAY_PROJECT_HOME"/queue/gated-audit-*.md \
  "$SPOOLWAY_PROJECT_HOME"/archive/gated-audit-*.md 2>/dev/null | head -1)
has "the task is queued with issue tracking off" "tracking: off" "${GATED_TASK:-/nonexistent}"
has "the dispatcher named the missing tool" "nosuchtool >= 1.0" "$GATED_LOG"
has "and said issue tracking is not supported for the firing" \
  "issue tracking is not supported." "$GATED_LOG"
lacks "without offering a key to press" "queue anyway" "$GATED_LOG"
# The job's own record, not any `fired_minute` in the file: the first section's
# job left one there that `fire_due` never prunes.
gated_minute() {
  jq -r '."gated-job".fired_minute // empty' "$SPOOLWAY_PROJECT_HOME/jobs.state.json" 2>/dev/null
}
# `fire_due` writes the state file only after its loop over jobs ends, which is
# after the task is saved, so the record can trail the task file by a moment.
gated_minute_recorded() { [ -n "$(gated_minute)" ]; }
poll_until 10 gated_minute_recorded
FIRED_MINUTE=$(gated_minute)
works "the minute is recorded as fired for the gated job itself" test -n "$FIRED_MINUTE"

# A later pass in the same minute finds the job already fired: the notice is
# named once for the minute, not again on every pass. `dispatch` draws no
# frame on a pass; it prints "next pass in" only when a pass ends idle. The
# firing pass may print one itself, so two after the firing line mean a later
# pass ran too. A fixed `sleep` could not show that.
passes_after_firing() {
  sed -n '/job gated-job · fired/,$p' "$GATED_LOG" | grep -ac "next pass in"
}
two_passes_after_firing() { [ "$(passes_after_firing)" -ge 2 ]; }
if poll_until 40 two_passes_after_firing; then
  ok "a later pass ran after the firing"
else
  bad "no later pass ended after the firing, so the once-per-minute check proves nothing"
fi
if [ "$(gated_minute)" = "$FIRED_MINUTE" ]; then
  works "the unmet tool is named once for the minute" \
    bash -c '[ "$(grep -ac "nosuchtool >= 1.0" "$1")" = 1 ]' _ "$GATED_LOG"
else
  # The clock crossed into the next minute, where the job legitimately fires
  # and names the tool again; one notice per recorded firing is the claim.
  works "the unmet tool is named once for each minute fired" \
    bash -c '[ "$(grep -ac "nosuchtool >= 1.0" "$1")" = 2 ]' _ "$GATED_LOG"
fi

# Stopped the way `dispatcher_stop` stops one: the board section below starts
# its own dispatcher, which is refused while this one holds the lock.
kill -TERM -- "-$(cat "$GATED_PID")" 2>/dev/null
poll_while 5 kill -0 -- "-$(cat "$GATED_PID")"
kill -KILL -- "-$(cat "$GATED_PID")" 2>/dev/null
poll_while 5 kill -0 -- "-$(cat "$GATED_PID")" \
  || bad "the pty dispatcher would not stop"
exec 8>&-
rm -f "$SPOOLWAY_PROJECT_HOME"/queue/gated-audit-*.md
must "the hook is cleared again for the rest of the suite" \
  "$SPOOLWAY" config set issue_tracking.hook ""
must "and the project key" "$SPOOLWAY" config set issue_tracking.project_key ""
must "and ticket slugs in names are back on, their default" \
  "$SPOOLWAY" config set issue_tracking.key_in_names true

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
routine = "weekly"
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

# A second enabled job on `nightly`, which `nightly-audit` already runs, and
# one on a single task file: `doctor` names each, and neither would fire.
cat >> "$SPOOLWAY_PROJECT_HOME/jobs.toml" <<TOML

[jobs.nightly-twin]
schedule = "0 9 * * *"
pipeline = "default"
routine = "nightly/"

[jobs.one-task]
schedule = "0 9 * * *"
pipeline = "default"
routine = "weekly/release-check.md"
TOML
says "a clash is named for the first job" \
  "job \`nightly-audit\` is the only job on its routine" "$SPOOLWAY" doctor
says "— naming the other job" "\`nightly-twin\`" "$SPOOLWAY" doctor
says "a job on a task file is named" \
  "job \`one-task\` routine is a whole routine folder" "$SPOOLWAY" doctor

finish
